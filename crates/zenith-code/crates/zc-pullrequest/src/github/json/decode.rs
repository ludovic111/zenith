//! The decoders of `gitHubPullRequestJson.ts` and the normalizers behind them: `gh` output in,
//! neutral provider types and contract types out.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use zc_contracts::prim::JsNumber;
use zc_contracts::{
    PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestChecksState, PullRequestComment, PullRequestCommentKind, PullRequestCommit,
    PullRequestDiffSide, PullRequestFileViewed, PullRequestFileViewedState, PullRequestLabel, PullRequestLabelCandidate, PullRequestLabelCandidateList,
    PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestMergeability, PullRequestOmittedFileStat, PullRequestReaction, PullRequestReactionContent,
    PullRequestReviewDecision, PullRequestReviewThread, PullRequestReviewerCandidate, PullRequestReviewerCandidateList, PullRequestReviewerKind,
    PullRequestStackMembership, PullRequestState, PullRequestThreadComment,
};
use zc_db::collate::locale_compare;
use zc_sourcecontrol::util::js_trim;

use super::raw::*;
use super::schema::{array, boolean, int, null_or, object, opt, opt_n, parse_json, req, req_n, string, unknown, DecodeFailure, Decoded};
use super::types::*;
use crate::checks::{dedupe_checks, CheckEntry};

/// `trimmed`: the text without its surrounding white space, `None` where nothing is left.
fn trimmed(value: Option<&str>) -> Option<String> {
    value.map(js_trim).filter(|text| !text.is_empty()).map(str::to_owned)
}

/// `value?.trim().toUpperCase()`.
fn upper(value: Option<&str>) -> Option<String> {
    value.map(|text| js_trim(text).to_uppercase())
}

/// `REACTION_CONTENT_BY_GITHUB`.
fn reaction_content_by_git_hub(name: &str) -> Option<PullRequestReactionContent> {
    Some(match name {
        "THUMBS_UP" => PullRequestReactionContent::ThumbsUp,
        "THUMBS_DOWN" => PullRequestReactionContent::ThumbsDown,
        "LAUGH" => PullRequestReactionContent::Laugh,
        "HOORAY" => PullRequestReactionContent::Hooray,
        "CONFUSED" => PullRequestReactionContent::Confused,
        "HEART" => PullRequestReactionContent::Heart,
        "ROCKET" => PullRequestReactionContent::Rocket,
        "EYES" => PullRequestReactionContent::Eyes,
        _ => return None,
    })
}

/// `gitHubReactionContent`: the contract's reaction as GitHub's `ReactionContent` enum names it.
pub fn git_hub_reaction_content(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "THUMBS_UP",
        PullRequestReactionContent::ThumbsDown => "THUMBS_DOWN",
        PullRequestReactionContent::Laugh => "LAUGH",
        PullRequestReactionContent::Hooray => "HOORAY",
        PullRequestReactionContent::Confused => "CONFUSED",
        PullRequestReactionContent::Heart => "HEART",
        PullRequestReactionContent::Rocket => "ROCKET",
        PullRequestReactionContent::Eyes => "EYES",
    }
}

/// `toReactions`: the groups somebody chose, as the contract carries them. The viewer's own login
/// is left out of `actors` (the page names them "You") but still counted.
fn to_reactions(groups: Option<&Vec<RawReactionGroup>>, viewer: Option<&str>) -> Vec<PullRequestReaction> {
    let normalized_viewer = viewer.map(str::to_lowercase);
    let mut reactions = Vec::new();
    for group in groups.into_iter().flatten() {
        let Some(content) = reaction_content_by_git_hub(&trimmed(group.content.as_deref()).map(|content| content.to_uppercase()).unwrap_or_default()) else {
            continue;
        };
        let logins: Vec<String> = group.logins.iter().filter_map(|login| trimmed(login.as_deref())).collect();
        #[allow(clippy::cast_possible_wrap)]
        let named = logins.len() as i64;
        let count = group.total_count.unwrap_or(named).max(named);
        if count <= 0 {
            continue;
        }
        let actors = match &normalized_viewer {
            None => logins,
            Some(viewer) => logins.into_iter().filter(|login| login.to_lowercase() != *viewer).collect(),
        };
        reactions.push(PullRequestReaction {
            content,
            count,
            actors,
            viewer_has_reacted: group.viewer_has_reacted == Some(true),
        });
    }
    reactions
}

/// `nextCursorOf`: the cursor only while the page says there is more (GitHub sends an
/// `endCursor` on the last page too).
fn next_cursor_of(page_info: Option<&RawPageInfo>) -> Option<String> {
    page_info
        .filter(|page_info| page_info.has_next_page == Some(true))
        .and_then(|page_info| trimmed(page_info.end_cursor.as_deref()))
}

/// `toPullRequestViewerFields`: updating is a permission (unknown grants it), authorship a fact
/// (unknown is "not the author").
fn to_pull_request_viewer_fields(raw: Option<RawViewerFields>) -> GitHubPullRequestViewerFields {
    GitHubPullRequestViewerFields {
        can_update: raw.and_then(|raw| raw.viewer_can_update) != Some(false),
        did_author: raw.and_then(|raw| raw.viewer_did_author) == Some(true),
    }
}

/// `toActor`: `None` for an actor with no login (a team, a mannequin).
fn to_actor(raw: Option<&RawActor>) -> Option<PullRequestActor> {
    let raw = raw?;
    let login = trimmed(raw.login.as_deref())?;
    Some(PullRequestActor {
        is_bot: (raw.typename.as_deref() == Some("Bot") || raw.is_bot == Some(true)).then_some(true),
        login,
        name: trimmed(raw.name.as_deref()),
        avatar_url: trimmed(raw.avatar_url.as_deref()),
    })
}

/// `toCommitActor`: an unlinked signature keeps its name (or email) as the login, so a
/// co-authored commit does not turn into one author.
fn to_commit_actor(raw: &RawCommitAuthor) -> Option<PullRequestActor> {
    let login = trimmed(raw.login.as_deref())
        .or_else(|| trimmed(raw.name.as_deref()))
        .or_else(|| trimmed(raw.email.as_deref()))?;
    Some(PullRequestActor {
        is_bot: None,
        login,
        name: trimmed(raw.name.as_deref()),
        avatar_url: None,
    })
}

/// `toGraphqlCommitActor`: an author off the GraphQL commits connection.
fn to_graphql_commit_actor(raw: &RawGraphqlCommitAuthor) -> Option<PullRequestActor> {
    let login = trimmed(raw.user_login.as_deref()).or_else(|| trimmed(raw.name.as_deref()))?;
    Some(PullRequestActor {
        is_bot: None,
        login,
        name: trimmed(raw.name.as_deref()),
        avatar_url: trimmed(raw.avatar_url.as_deref()),
    })
}

/// `toState`: a merge time outranks whatever the state says.
fn to_state(state: Option<&str>, merged_at: Option<&str>) -> PullRequestState {
    if trimmed(merged_at).is_some() {
        return PullRequestState::Merged;
    }
    match upper(state).as_deref() {
        Some("MERGED") => PullRequestState::Merged,
        Some("CLOSED") => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

fn to_mergeability(value: Option<&str>) -> PullRequestMergeability {
    match upper(value).as_deref() {
        Some("MERGEABLE") => PullRequestMergeability::Mergeable,
        Some("CONFLICTING") => PullRequestMergeability::Conflicting,
        _ => PullRequestMergeability::Unknown,
    }
}

fn to_merge_method(value: Option<&str>) -> Option<PullRequestMergeMethod> {
    match upper(value).as_deref() {
        Some("MERGE") => Some(PullRequestMergeMethod::Merge),
        Some("SQUASH") => Some(PullRequestMergeMethod::Squash),
        Some("REBASE") => Some(PullRequestMergeMethod::Rebase),
        _ => None,
    }
}

fn to_review_decision(value: Option<&str>) -> Option<PullRequestReviewDecision> {
    match upper(value).as_deref() {
        Some("APPROVED") => Some(PullRequestReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(PullRequestReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(PullRequestReviewDecision::ReviewRequired),
        _ => None,
    }
}

/// `toReviewDecisionWithReviews`: GitHub's `reviewDecision` counts only reviews that satisfy the
/// branch rules (not a bot's approval); where it has no verdict, the latest review per reviewer
/// decides, changes requested outranking approval.
fn to_review_decision_with_reviews<'a>(
    value: Option<&str>,
    latest_reviews: impl IntoIterator<Item = &'a RawLatestReview>,
) -> Option<PullRequestReviewDecision> {
    let summarized = to_review_decision(value);
    if matches!(
        summarized,
        Some(PullRequestReviewDecision::Approved | PullRequestReviewDecision::ChangesRequested)
    ) {
        return summarized;
    }
    let states: BTreeSet<String> = latest_reviews
        .into_iter()
        .map(|review| upper(review.state.as_deref()).unwrap_or_default())
        .collect();
    if states.contains("CHANGES_REQUESTED") {
        return Some(PullRequestReviewDecision::ChangesRequested);
    }
    if states.contains("APPROVED") {
        return Some(PullRequestReviewDecision::Approved);
    }
    summarized
}

fn to_labels(raw: Option<&Vec<RawLabel>>) -> Vec<PullRequestLabel> {
    raw.into_iter()
        .flatten()
        .filter_map(|label| {
            Some(PullRequestLabel {
                name: trimmed(Some(&label.name))?,
                color: trimmed(label.color.as_deref()),
            })
        })
        .collect()
}

/// `toReviewRequestLogins`: user requests only (a team slug is not a login).
fn to_review_request_logins(raw: Option<&Vec<RawReviewRequest>>) -> Vec<String> {
    raw.into_iter().flatten().filter_map(|request| trimmed(request.login.as_deref())).collect()
}

fn has_team_review_request(raw: Option<&Vec<RawReviewRequest>>) -> bool {
    raw.into_iter().flatten().any(|request| {
        trimmed(request.login.as_deref()).is_none() && (trimmed(request.slug.as_deref()).is_some() || trimmed(request.name.as_deref()).is_some())
    })
}

/// `toCheckStatus`: a check run reports `status` and, once completed, a `conclusion`; a commit
/// status reports one `state`.
fn to_check_status(raw: &RawCheck) -> PullRequestCheckStatus {
    if let Some(status) = upper(raw.status.as_deref()) {
        if status != "COMPLETED" && !status.is_empty() {
            return PullRequestCheckStatus::Pending;
        }
    }
    match upper(raw.conclusion.as_deref().or(raw.state.as_deref())).as_deref() {
        Some("SUCCESS") => PullRequestCheckStatus::Success,
        Some("ACTION_REQUIRED") => PullRequestCheckStatus::ActionRequired,
        Some("FAILURE" | "ERROR" | "TIMED_OUT" | "STARTUP_FAILURE") => PullRequestCheckStatus::Failure,
        Some("CANCELLED") => PullRequestCheckStatus::Cancelled,
        Some("SKIPPED") => PullRequestCheckStatus::Skipped,
        Some("PENDING" | "EXPECTED") => PullRequestCheckStatus::Pending,
        _ => PullRequestCheckStatus::Neutral,
    }
}

/// `UNSET_TIMESTAMP`: what GitHub writes where a run has not reached that moment yet.
const UNSET_TIMESTAMP: &str = "0001-01-01T00:00:00Z";

fn real_timestamp(value: Option<&str>) -> Option<String> {
    trimmed(value).filter(|at| at != UNSET_TIMESTAMP)
}

fn is_nameless_check(raw: &RawCheck) -> bool {
    trimmed(raw.name.as_deref()).is_none() && trimmed(raw.context.as_deref()).is_none()
}

/// `toCheckEntries`: each named check with its workflow and when the run last had something to say.
fn to_check_entries(raw: Option<&Vec<RawCheck>>) -> Vec<CheckEntry> {
    raw.into_iter()
        .flatten()
        .filter_map(|check| {
            let name = trimmed(check.name.as_deref()).or_else(|| trimmed(check.context.as_deref()))?;
            Some(CheckEntry {
                check: PullRequestCheck {
                    name,
                    status: to_check_status(check),
                    description: trimmed(check.description.as_deref()),
                    url: trimmed(check.details_url.as_deref()).or_else(|| trimmed(check.target_url.as_deref())),
                },
                workflow_name: trimmed(check.workflow_name.as_deref()),
                at: real_timestamp(check.completed_at.as_deref()).or_else(|| real_timestamp(check.started_at.as_deref())),
            })
        })
        .collect()
}

fn to_checks(raw: Option<&Vec<RawCheck>>) -> Vec<PullRequestCheck> {
    dedupe_checks(&to_check_entries(raw))
}

/// `rollupChecksState`: the one word a row has room for, counted off the deduped checks plus any
/// nameless row (the search's rollup enum). A failure (or cancellation) outranks a run still
/// going; `None` for no checks at all, or nothing that passed.
fn rollup_checks_state(raw: Option<&Vec<RawCheck>>) -> Option<PullRequestChecksState> {
    let statuses: Vec<PullRequestCheckStatus> = to_checks(raw)
        .iter()
        .map(|check| check.status)
        .chain(raw.into_iter().flatten().filter(|check| is_nameless_check(check)).map(to_check_status))
        .collect();
    if statuses.is_empty() {
        return None;
    }
    let has = |status| statuses.contains(&status);
    if has(PullRequestCheckStatus::Failure) || has(PullRequestCheckStatus::Cancelled) {
        return Some(PullRequestChecksState::Failing);
    }
    if has(PullRequestCheckStatus::Pending) || has(PullRequestCheckStatus::ActionRequired) {
        return Some(PullRequestChecksState::Pending);
    }
    has(PullRequestCheckStatus::Success).then_some(PullRequestChecksState::Passing)
}

/// `isReviewVerdict`: the states that are a verdict in themselves.
fn is_review_verdict(review_state: Option<&str>) -> bool {
    matches!(
        review_state.map(str::to_uppercase).as_deref(),
        Some("APPROVED" | "CHANGES_REQUESTED" | "DISMISSED")
    )
}

/// `toComments`: issue comments and reviews in time order. A bodiless review is kept only when
/// its state is the event itself; the bodiless `COMMENTED` container of line comments is not.
fn to_comments(comments: Option<&Vec<RawComment>>, reviews: Option<&Vec<RawReview>>) -> Vec<PullRequestComment> {
    let issue_comments = comments.into_iter().flatten().map(|comment| PullRequestComment {
        id: comment.id.clone(),
        kind: PullRequestCommentKind::IssueComment,
        author: to_actor(comment.author.as_ref()),
        body: comment.body.clone().unwrap_or_default(),
        created_at: comment.created_at.clone(),
        url: trimmed(comment.url.as_deref()),
        path: None,
        review_state: None,
        reactions: None,
    });
    let reviews = reviews.into_iter().flatten().filter_map(|review| {
        let submitted_at = trimmed(review.submitted_at.as_deref())?;
        let review_state = trimmed(review.state.as_deref());
        if js_trim(review.body.as_deref().unwrap_or_default()).is_empty() && !is_review_verdict(review_state.as_deref()) {
            return None;
        }
        Some(PullRequestComment {
            id: review.id.clone(),
            kind: PullRequestCommentKind::Review,
            author: to_actor(review.author.as_ref()),
            body: review.body.clone().unwrap_or_default(),
            created_at: submitted_at,
            url: trimmed(review.url.as_deref()),
            path: None,
            review_state,
            reactions: None,
        })
    });
    let mut all: Vec<PullRequestComment> = issue_comments.chain(reviews).collect();
    all.sort_by(|left, right| locale_compare(&left.created_at, &right.created_at));
    all
}

fn to_commits(commits: Option<&Vec<RawCommit>>) -> Vec<PullRequestCommit> {
    commits
        .into_iter()
        .flatten()
        .map(|commit| PullRequestCommit {
            oid: commit.oid.clone(),
            message_headline: commit.message_headline.clone().unwrap_or_default(),
            committed_date: commit.committed_date.clone(),
            additions: None,
            deletions: None,
            authors: Some(commit.authors.iter().flatten().filter_map(to_commit_actor).collect()),
        })
        .collect()
}

fn to_list_item(raw: &RawListItem) -> GitHubPullRequestListItem {
    GitHubPullRequestListItem {
        stack: None,
        author_id: trimmed(raw.author.as_ref().and_then(|author| author.id.as_deref())),
        number: raw.number,
        title: raw.title.clone(),
        url: raw.url.clone(),
        author: to_actor(raw.author.as_ref()),
        head_branch: raw.head_ref_name.clone(),
        base_branch: raw.base_ref_name.clone(),
        state: to_state(raw.state.as_deref(), raw.merged_at.as_deref()),
        is_draft: raw.is_draft.unwrap_or(false),
        mergeability: to_mergeability(raw.mergeable.as_deref()),
        review_decision: to_review_decision_with_reviews(raw.review_decision.as_deref(), raw.latest_reviews.iter().flatten()),
        additions: raw.additions.unwrap_or(0),
        deletions: raw.deletions.unwrap_or(0),
        created_at: raw.created_at.clone(),
        updated_at: raw.updated_at.clone(),
        review_request_logins: to_review_request_logins(raw.review_requests.as_ref()),
        has_team_review_request: has_team_review_request(raw.review_requests.as_ref()),
        labels: to_labels(raw.labels.as_ref()),
        checks_state: rollup_checks_state(raw.status_check_rollup.as_ref()),
    }
}

fn to_detail(raw: &RawDetail) -> GitHubPullRequestDetail {
    GitHubPullRequestDetail {
        item: to_list_item(&raw.item),
        is_cross_repository: raw.is_cross_repository,
        head_repository_owner: trimmed(raw.head_repository_owner.as_deref()),
        head_sha: trimmed(raw.head_ref_oid.as_deref()),
        body: raw.body.clone().unwrap_or_default(),
        changed_files: raw.changed_files.unwrap_or(0),
        merged_at: trimmed(raw.item.merged_at.as_deref()),
        closed_at: trimmed(raw.closed_at.as_deref()),
        checks: to_checks(raw.item.status_check_rollup.as_ref()),
        // A JSON null is GitHub saying "nobody armed this"; a missing key is GitHub not saying.
        auto_merge_enabled: raw.auto_merge_request.as_ref().map(Option::is_some),
        auto_merge_method: to_merge_method(
            raw.auto_merge_request
                .as_ref()
                .and_then(|request| request.as_ref())
                .and_then(|method| method.as_deref()),
        ),
    }
}

/// `^s(\d+)$`: the position an aliased answer was asked in.
fn alias_index(alias: &str) -> Option<usize> {
    let digits = alias.strip_prefix('s')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `toStackMembership`: both halves or nothing.
fn to_stack_membership(raw: &RawStackMembership) -> Option<PullRequestStackMembership> {
    let (number, size, base) = raw.stack.clone()?;
    Some(PullRequestStackMembership {
        number,
        size,
        base,
        position: raw.stack_entry_position?,
    })
}

/// The search's `{ state }` rollup enum dressed as nameless checks, so it rolls up like a listing's.
fn rollup_state_checks(states: Option<&Vec<Option<Option<String>>>>) -> Vec<RawCheck> {
    states
        .into_iter()
        .flatten()
        .filter_map(|state| trimmed(state.as_ref().and_then(Option::as_deref)))
        .map(|state| RawCheck {
            state: Some(state),
            ..RawCheck::default()
        })
        .collect()
}

fn non_null_latest_reviews(reviews: Option<&Vec<Option<RawLatestReview>>>) -> Vec<RawLatestReview> {
    reviews.into_iter().flatten().flatten().cloned().collect()
}

/// `decodeActorAvatarsJson`: avatars by login, for the authors a listing names without one.
pub fn decode_actor_avatars_json(raw: &str) -> Result<BTreeMap<String, String>, DecodeFailure> {
    let value = parse_json(raw)?;
    let nodes = at_path(&value, &["data", "nodes"], &|nodes| array(nodes, |node| null_or(node, raw_actor)))?;
    let mut avatars_by_login = BTreeMap::new();
    for node in nodes.iter().flatten() {
        if let (Some(login), Some(avatar_url)) = (trimmed(node.login.as_deref()), trimmed(node.avatar_url.as_deref())) {
            avatars_by_login.insert(login, avatar_url);
        }
    }
    Ok(avatars_by_login)
}

/// `decodePullRequestPreviewJson`.
pub fn decode_pull_request_preview_json(raw: &str) -> Result<GitHubPullRequestPreview, DecodeFailure> {
    let value = parse_json(raw)?;
    at_path(&value, &["data", "repository", "pullRequest"], &|pull_request| {
        let map = object(pull_request)?;
        let number = req(map, "number", int)?;
        let title = req(map, "title", string)?;
        let url = req(map, "url", string)?;
        let state = req(map, "state", string)?;
        let is_draft = req(map, "isDraft", boolean)?;
        let created_at = req(map, "createdAt", string)?;
        let author = req_n(map, "author", raw_actor)?;
        Ok(GitHubPullRequestPreview {
            number,
            title,
            url,
            state: to_state(Some(&state), None),
            is_draft,
            created_at,
            author: to_actor(author.as_ref()),
        })
    })
}

/// `decodePullRequestNodeIdJson`: the pull request's own node id.
pub fn decode_pull_request_node_id_json(raw: &str) -> Result<String, DecodeFailure> {
    at_path(&parse_json(raw)?, &["data", "repository", "pullRequest"], &id_object)
}

/// `decodeReactionSubjectScopeJson`: true when the subject is the pull request itself or hangs
/// off it; false for anything else, including what this host could not find.
pub fn decode_reaction_subject_scope_json(raw: &str) -> Result<bool, DecodeFailure> {
    let value = parse_json(raw)?;
    let (expected, actual) = req(object(&value)?, "data", |data| {
        let data = object(data)?;
        let expected = req_n(data, "repository", |repository| req_n(object(repository)?, "pullRequest", id_object))?.flatten();
        let node = req_n(data, "node", |node| {
            let node = object(node)?;
            let id = req(node, "id", string)?;
            let pull_request = opt(node, "pullRequest", id_object)?;
            Ok(pull_request.unwrap_or(id))
        })?;
        Ok((expected, node))
    })?;
    Ok(matches!((expected, actual), (Some(expected), Some(actual)) if expected == actual))
}

/// `decodePullRequestListJson`: malformed rows are skipped (one surprise must not blank the
/// list) but still counted, so paging does not stop early.
pub fn decode_pull_request_list_json(raw: &str) -> Result<GitHubPullRequestListBatch, DecodeFailure> {
    let entries = array(&parse_json(raw)?, unknown)?;
    let items = entries
        .iter()
        .filter_map(|entry| raw_list_item(entry).ok())
        .map(|item| to_list_item(&item))
        .collect();
    Ok(GitHubPullRequestListBatch {
        items,
        raw_count: entries.len(),
    })
}

/// `decodePullRequestSearchJson`: a search row flattened to the listing's shape, its rollup enum
/// dressed as one nameless check. Rows that are not pull requests (or name no repository) are
/// skipped but counted.
pub fn decode_pull_request_search_json(raw: &str) -> Result<GitHubPullRequestSearchBatch, DecodeFailure> {
    let search = raw_search(&parse_json(raw)?)?;
    let nodes = search.nodes.unwrap_or_default();
    let mut items = Vec::new();
    for entry in &nodes {
        let Ok(node) = raw_search_item(entry) else { continue };
        let Some(repository) = trimmed(node.repository.as_deref()) else { continue };
        let stack = to_stack_membership(&node.membership);
        let list_item = RawListItem {
            number: node.number,
            title: node.title.clone(),
            url: node.url.clone(),
            author: node.author.clone(),
            head_ref_name: node.head_ref_name.clone(),
            base_ref_name: node.base_ref_name.clone(),
            state: node.state.clone(),
            is_draft: node.is_draft,
            mergeable: node.mergeable.clone(),
            review_decision: node.review_decision.clone(),
            additions: None,
            deletions: None,
            created_at: node.created_at.clone().unwrap_or_default(),
            updated_at: node.updated_at.clone(),
            merged_at: node.merged_at.clone(),
            review_requests: Some(
                node.review_requests
                    .iter()
                    .flatten()
                    .filter_map(|request| {
                        trimmed(
                            request
                                .as_ref()
                                .and_then(|reviewer| reviewer.as_ref())
                                .and_then(|reviewer| reviewer.login.as_deref()),
                        )
                    })
                    .map(|login| RawReviewRequest {
                        login: Some(login),
                        ..RawReviewRequest::default()
                    })
                    .collect(),
            ),
            latest_reviews: Some(non_null_latest_reviews(node.latest_reviews.as_ref())),
            labels: Some(node.labels.iter().flatten().flatten().cloned().collect()),
            status_check_rollup: Some(rollup_state_checks(node.rollup_states.as_ref())),
        };
        items.push(GitHubPullRequestSearchItem {
            item: GitHubPullRequestListItem {
                stack,
                ..to_list_item(&list_item)
            },
            repository,
        });
    }
    Ok(GitHubPullRequestSearchBatch {
        items,
        raw_count: nodes.len(),
        has_next_page: search.has_next_page.unwrap_or(false),
    })
}

/// `decodePullRequestStackMembershipsJson`: memberships by the position they were asked in;
/// a missing pull request or a half membership is absent.
pub fn decode_pull_request_stack_memberships_json(raw: &str) -> Result<BTreeMap<usize, PullRequestStackMembership>, DecodeFailure> {
    let entries = aliased_data(&parse_json(raw)?, false, |value| {
        null_or(value, |entry| req_n(object(entry)?, "pullRequest", raw_stack_membership))
    })?;
    let mut memberships = BTreeMap::new();
    for (alias, value) in entries {
        let (Some(index), Some(Some(pull_request))) = (alias_index(&alias), value) else {
            continue;
        };
        if let Some(stack) = to_stack_membership(&pull_request) {
            memberships.insert(index, stack);
        }
    }
    Ok(memberships)
}

/// `decodePullRequestStatsJson`: line counts by the position they were asked in; a repository or
/// pull request GitHub answered nothing for is absent.
pub fn decode_pull_request_stats_json(raw: &str) -> Result<BTreeMap<usize, GitHubLineStats>, DecodeFailure> {
    let entries = aliased_data(&parse_json(raw)?, true, |value| {
        null_or(value, |entry| {
            opt_n(object(entry)?, "pullRequest", |pull_request| {
                let map = object(pull_request)?;
                Ok(GitHubLineStats {
                    additions: opt_n(map, "additions", int)?.unwrap_or(0),
                    deletions: opt_n(map, "deletions", int)?.unwrap_or(0),
                })
            })
        })
    })?;
    let mut stats = BTreeMap::new();
    for (alias, value) in entries {
        if let (Some(index), Some(Some(pull_request))) = (alias_index(&alias), value) {
            stats.insert(index, pull_request);
        }
    }
    Ok(stats)
}

/// `decodePullRequestSummariesJson`: summaries by the position they were asked in. A pull
/// request GitHub answered nothing for, or whose fields no longer decode, is absent.
pub fn decode_pull_request_summaries_json(raw: &str) -> Result<BTreeMap<usize, GitHubPullRequestSummary>, DecodeFailure> {
    let entries = aliased_data(&parse_json(raw)?, true, |value| {
        null_or(value, |entry| opt(object(entry)?, "pullRequest", unknown))
    })?;
    let mut summaries = BTreeMap::new();
    for (alias, value) in entries {
        let (Some(index), Some(Some(pull_request))) = (alias_index(&alias), value) else {
            continue;
        };
        if pull_request.is_null() {
            continue;
        }
        let Ok(summary) = raw_summary(&pull_request) else { continue };
        let pr = &summary.item;
        summaries.insert(
            index,
            GitHubPullRequestSummary {
                number: pr.number,
                title: pr.title.clone(),
                url: pr.url.clone(),
                head_branch: pr.head_ref_name.clone(),
                base_branch: pr.base_ref_name.clone(),
                state: to_state(pr.state.as_deref(), pr.merged_at.as_deref()),
                is_draft: pr.is_draft.unwrap_or(false),
                closed_at: trimmed(summary.closed_at.as_deref()),
                merged_at: trimmed(pr.merged_at.as_deref()),
                updated_at: pr.updated_at.clone(),
                author: to_actor(pr.author.as_ref()),
                additions: summary.additions.unwrap_or(0),
                deletions: summary.deletions.unwrap_or(0),
                changed_files: summary.changed_files.unwrap_or(0),
                review_decision: to_review_decision_with_reviews(pr.review_decision.as_deref(), &non_null_latest_reviews(pr.latest_reviews.as_ref())),
                checks_state: rollup_checks_state(Some(&rollup_state_checks(pr.rollup_states.as_ref()))),
                mergeability: to_mergeability(pr.mergeable.as_deref()),
            },
        );
    }
    Ok(summaries)
}

/// `toCanWrite`: a role that can push. An unreported permission is not write: offering a merge
/// a reader cannot use is the worse failure.
fn to_can_write(viewer_permission: Option<&str>) -> bool {
    matches!(upper(viewer_permission).as_deref(), Some("ADMIN" | "MAINTAIN" | "WRITE"))
}

/// `toCanTriage`: triage, the least role that may label, or anything that can write.
fn to_can_triage(viewer_permission: Option<&str>) -> bool {
    upper(viewer_permission).as_deref() == Some("TRIAGE") || to_can_write(viewer_permission)
}

fn merge_capabilities(access: &RawRepositoryAccess) -> PullRequestMergeCapabilities {
    PullRequestMergeCapabilities {
        merge: access.merge_commit_allowed,
        squash: access.squash_merge_allowed,
        rebase: access.rebase_merge_allowed,
    }
}

fn viewer_repository_access(access: &RawRepositoryAccess, viewer: Option<RawViewerFields>) -> GitHubViewerRepositoryAccess {
    let fields = to_pull_request_viewer_fields(viewer);
    GitHubViewerRepositoryAccess {
        viewer: GitHubViewerAccess {
            can_write: to_can_write(access.viewer_permission.as_deref()),
            can_triage: to_can_triage(access.viewer_permission.as_deref()),
            can_update: fields.can_update,
            did_author: fields.did_author,
            can_update_branch: None,
        },
        merge_capabilities: merge_capabilities(access),
    }
}

/// One `contexts.nodes` entry of the core read: a check with its workflow's name.
fn core_check(value: &Value) -> Decoded<RawCheck> {
    let map = object(value)?;
    let mut check = check_fields(map)?;
    let workflow = opt_n(map, "checkSuite", |suite| {
        req_n(object(suite)?, "workflowRun", |run| {
            req_n(object(run)?, "workflow", |workflow| req(object(workflow)?, "name", string))
        })
    })?;
    check.workflow_name = workflow.flatten().flatten();
    Ok(check)
}

/// `{ nodes, pageInfo: { hasNextPage } }` of the core read's check contexts.
fn core_contexts(value: &Value) -> Decoded<(Vec<RawCheck>, bool)> {
    let map = object(value)?;
    let nodes = req(map, "nodes", |nodes| array(nodes, core_check))?;
    let has_next_page = req(map, "pageInfo", |page| req(object(page)?, "hasNextPage", boolean))?;
    Ok((nodes, has_next_page))
}

/// `decodePullRequestCoreJson`: the detail, the viewer's standing, the merge settings and the
/// base comparison of one read.
pub fn decode_pull_request_core_json(raw: &str) -> Result<GitHubPullRequestCore, DecodeFailure> {
    let value = parse_json(raw)?;
    at_path(&value, &["data", "repository"], &|repository| {
        let repository = object(repository)?;
        let access = repository_access_fields(repository)?;
        req(repository, "pullRequest", |pull_request| {
            let map = object(pull_request)?;
            // The core's connections replace the list's flat `reviewRequests` and `labels` in place.
            let item = list_item_with(
                map,
                &|map| {
                    let requests = req(map, "reviewRequests", |requests| {
                        req(object(requests)?, "nodes", |nodes| {
                            array(nodes, |node| req_n(object(node)?, "requestedReviewer", raw_review_request))
                        })
                    })?;
                    Ok(Some(requests.into_iter().flatten().collect()))
                },
                &|map| {
                    Ok(Some(req(map, "labels", |labels| {
                        req(object(labels)?, "nodes", |nodes| array(nodes, raw_label))
                    })?))
                },
            )?;
            let mut detail = detail_extra_fields(map, item)?;
            let viewer = viewer_fields(map)?;
            let viewer_can_update_branch = req(map, "viewerCanUpdateBranch", boolean)?;
            let behind_by = req_n(map, "baseRef", |base_ref| {
                req_n(object(base_ref)?, "compare", |compare| req(object(compare)?, "behindBy", int))
            })?;
            let commits = req(map, "commits", |commits| {
                req(object(commits)?, "nodes", |nodes| {
                    array(nodes, |node| {
                        req(object(node)?, "commit", |commit| {
                            req_n(object(commit)?, "statusCheckRollup", |rollup| req(object(rollup)?, "contexts", core_contexts))
                        })
                    })
                })
            })?;
            let contexts = commits.into_iter().next().flatten();
            let checks_truncated = contexts.as_ref().is_some_and(|(_, has_next_page)| *has_next_page);
            let state = detail.item.state.clone();
            detail.item.status_check_rollup = Some(contexts.map(|(nodes, _)| nodes).unwrap_or_default());
            Ok(GitHubPullRequestCore {
                detail: to_detail(&detail),
                viewer_access: viewer_repository_access(&access, Some(viewer)),
                comparison: match (state.as_deref(), behind_by) {
                    #[allow(clippy::cast_precision_loss)]
                    (Some("OPEN"), Some(Some(behind_by))) => Some(GitHubBaseComparison {
                        behind_by: Some(JsNumber(behind_by as f64)),
                        viewer_can_update: viewer_can_update_branch,
                    }),
                    _ => None,
                },
                checks_truncated,
            })
        })
    })
}

/// `decodePullRequestDetailJson`.
pub fn decode_pull_request_detail_json(raw: &str) -> Result<GitHubPullRequestDetail, DecodeFailure> {
    Ok(to_detail(&raw_detail(&parse_json(raw)?)?))
}

/// `decodeWorkflowRunApprovalsJson`: the runs waiting for approval, named after their workflow.
pub fn decode_workflow_run_approvals_json(raw: &str) -> Result<Vec<GitHubWorkflowRunApproval>, DecodeFailure> {
    let runs = array(&parse_json(raw)?, raw_workflow_run_approval)?;
    Ok(runs
        .into_iter()
        .map(|run| GitHubWorkflowRunApproval {
            id: run.database_id,
            name: trimmed(run.workflow_name.as_deref()).unwrap_or_else(|| format!("Workflow run {}", run.database_id)),
            url: trimmed(run.url.as_deref()),
        })
        .collect())
}

/// `decodePullRequestHeadsJson`.
pub fn decode_pull_request_heads_json(raw: &str) -> Result<Vec<GitHubPullRequestHead>, DecodeFailure> {
    let heads = array(&parse_json(raw)?, raw_pull_request_head)?;
    Ok(heads
        .into_iter()
        .map(|head| GitHubPullRequestHead {
            number: head.number,
            head_sha: head.head_ref_oid,
            is_cross_repository: head.is_cross_repository,
            head_repository_owner: trimmed(head.head_repository_owner.as_deref()),
        })
        .collect())
}

/// `decodePullRequestActivityJson`.
pub fn decode_pull_request_activity_json(raw: &str) -> Result<GitHubPullRequestActivity, DecodeFailure> {
    let activity = raw_activity(&parse_json(raw)?)?;
    Ok(GitHubPullRequestActivity {
        author: to_actor(activity.author.as_ref()),
        comments: to_comments(activity.comments.as_ref(), activity.reviews.as_ref()),
        commits: to_commits(activity.commits.as_ref()),
    })
}

/// `reviewThreadConversation`: the threads as one flat conversation, resolved ones and every
/// reply included.
pub fn review_thread_conversation(threads: &[PullRequestReviewThread]) -> Vec<PullRequestComment> {
    threads
        .iter()
        .flat_map(|thread| {
            thread.comments.iter().map(|comment| PullRequestComment {
                id: comment.id.clone(),
                kind: PullRequestCommentKind::ReviewComment,
                author: comment.author.clone(),
                body: comment.body.clone(),
                created_at: comment.created_at.clone(),
                url: comment.url.clone(),
                path: Some(thread.path.clone()),
                review_state: None,
                reactions: Some(comment.reactions.clone().unwrap_or_default()),
            })
        })
        .collect()
}

/// `toDismissalEntries`.
fn to_dismissal_entries(nodes: Option<&Vec<RawDismissal>>) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    for node in nodes.into_iter().flatten() {
        if let (Some(review_id), Some(message)) = (trimmed(node.review_id.as_deref()), trimmed(node.message.as_deref())) {
            entries.insert(review_id, message);
        }
    }
    entries
}

/// `decodeReviewDismissalsJson`: one further page of dismissal events.
pub fn decode_review_dismissals_json(raw: &str) -> Result<GitHubReviewDismissalsPage, DecodeFailure> {
    let (page_info, nodes) = at_path(&parse_json(raw)?, &["data", "repository", "pullRequest", "timelineItems"], &raw_dismissals)?;
    Ok(GitHubReviewDismissalsPage {
        dismissals_by_review_id: to_dismissal_entries(Some(&nodes)),
        next_cursor: next_cursor_of(page_info.as_ref()),
    })
}

fn to_thread_comment(comment: &RawComment, viewer: Option<&str>) -> PullRequestThreadComment {
    PullRequestThreadComment {
        id: comment.id.clone(),
        author: to_actor(comment.author.as_ref()),
        body: comment.body.clone().unwrap_or_default(),
        created_at: comment.created_at.clone(),
        url: trimmed(comment.url.as_deref()),
        reactions: Some(to_reactions(comment.reaction_groups.as_ref(), viewer)),
    }
}

/// `decodeReviewThreadsJson`: one page of review threads with everything that rides along.
/// Following the cursors it hands back is the caller's job.
pub fn decode_review_threads_json(raw: &str) -> Result<GitHubReviewThreadPage, DecodeFailure> {
    let decoded = raw_review_threads(&parse_json(raw)?)?;
    let viewer = trimmed(decoded.viewer.as_deref());
    let viewer = viewer.as_deref();
    let pull_request = &decoded.pull_request;
    let threads = pull_request
        .threads
        .iter()
        .filter_map(|thread| {
            let path = trimmed(thread.path.as_deref())?;
            let id = trimmed(thread.id.as_deref())?;
            if thread.comments.nodes.is_empty() {
                return None;
            }
            Some(GitHubReviewThreadEntry {
                thread: PullRequestReviewThread {
                    id,
                    path,
                    // Null once the thread's line has left the diff: listed rather than pinned.
                    line: thread.line.filter(|line| *line > 0),
                    side: if thread.diff_side.as_deref().map(str::to_uppercase).as_deref() == Some("LEFT") {
                        PullRequestDiffSide::Left
                    } else {
                        PullRequestDiffSide::Right
                    },
                    is_resolved: thread.is_resolved == Some(true),
                    is_outdated: thread.is_outdated == Some(true),
                    comments: thread.comments.nodes.iter().map(|comment| to_thread_comment(comment, viewer)).collect(),
                    comment_count: None,
                    next_comments_cursor: None,
                },
                #[allow(clippy::cast_possible_wrap)]
                comment_count: thread.comments.total_count.unwrap_or(thread.comments.nodes.len() as i64),
                next_comment_cursor: next_cursor_of(thread.comments.page_info.as_ref()),
            })
        })
        .collect();
    let review_requests: Vec<Option<&RawActor>> = pull_request.review_requests.iter().flatten().map(Option::as_ref).collect();
    let latest_review_authors: Vec<Option<&RawActor>> = pull_request.latest_reviews.iter().flatten().map(|review| review.author.as_ref()).collect();
    let mut avatars_by_login = BTreeMap::new();
    let mut bot_logins = BTreeSet::new();
    let everyone = std::iter::once(pull_request.author.as_ref())
        .chain(pull_request.comments.iter().flatten().map(|node| node.author.as_ref()))
        .chain(pull_request.reviews.iter().flatten().map(|node| node.author.as_ref()))
        .chain(review_requests.iter().copied())
        .chain(latest_review_authors.iter().copied())
        .chain(
            pull_request
                .threads
                .iter()
                .flat_map(|thread| thread.comments.nodes.iter().map(|comment| comment.author.as_ref())),
        );
    for raw in everyone {
        let login = trimmed(raw.and_then(|raw| raw.login.as_deref()));
        let avatar_url = trimmed(raw.and_then(|raw| raw.avatar_url.as_deref()));
        if let (Some(login), Some(avatar_url)) = (&login, avatar_url) {
            avatars_by_login.insert(login.clone(), avatar_url);
        }
        if let Some(login) = login {
            if to_actor(raw).and_then(|actor| actor.is_bot) == Some(true) {
                bot_logins.insert(login);
            }
        }
    }
    // Keyed by login, so someone who was asked and then answered appears once.
    let mut reviewers: Vec<PullRequestActor> = Vec::new();
    for raw in review_requests.iter().chain(latest_review_authors.iter()) {
        if let Some(actor) = to_actor(*raw) {
            if !reviewers.iter().any(|reviewer| reviewer.login == actor.login) {
                reviewers.push(actor);
            }
        }
    }
    let mut commit_stats = BTreeMap::new();
    let mut commits = Vec::new();
    for commit in pull_request.commits.iter().flatten() {
        let Some(oid) = trimmed(Some(&commit.oid)) else { continue };
        // A merge commit is measured against its first parent, which counts every upstream
        // change as the pull request's own: no useful stat to show for it.
        if commit.parents_total_count.unwrap_or(1) <= 1 {
            if let (Some(additions), Some(deletions)) = (commit.additions, commit.deletions) {
                commit_stats.insert(
                    oid.clone(),
                    GitHubLineStats {
                        additions: additions.max(0),
                        deletions: deletions.max(0),
                    },
                );
            }
        }
        let Some(committed_date) = trimmed(commit.committed_date.as_deref()) else {
            continue;
        };
        commits.push(PullRequestCommit {
            oid,
            message_headline: commit.message_headline.clone().unwrap_or_default(),
            committed_date,
            additions: None,
            deletions: None,
            authors: Some(commit.authors.iter().flatten().filter_map(to_graphql_commit_actor).collect()),
        });
    }
    let mut reactions_by_id = BTreeMap::new();
    for node in pull_request.comments.iter().flatten().chain(pull_request.reviews.iter().flatten()) {
        let Some(id) = trimmed(node.id.as_deref()) else { continue };
        let reactions = to_reactions(node.reaction_groups.as_ref(), viewer);
        if !reactions.is_empty() {
            reactions_by_id.insert(id, reactions);
        }
    }
    Ok(GitHubReviewThreadPage {
        threads,
        next_cursor: next_cursor_of(pull_request.threads_page_info.as_ref()),
        reactions: to_reactions(pull_request.reaction_groups.as_ref(), viewer),
        reactions_by_id,
        reviewers,
        avatars_by_login,
        bot_logins,
        commit_stats,
        commits,
        viewer: to_pull_request_viewer_fields(Some(pull_request.viewer)),
        dismissals_by_review_id: to_dismissal_entries(pull_request.dismissals.as_ref()),
        next_dismissal_cursor: next_cursor_of(pull_request.dismissals_page_info.as_ref()),
    })
}

/// `decodeReviewThreadCommentsJson`: the rest of one thread's comments, in the shape the first
/// page delivered them, and whether the thread belongs to the pull request asked about.
pub fn decode_review_thread_comments_json(raw: &str) -> Result<GitHubReviewThreadCommentsPage, DecodeFailure> {
    let value = parse_json(raw)?;
    req(object(&value)?, "data", |data| {
        let data = object(data)?;
        let viewer = trimmed(viewer_login(data)?.as_deref());
        let expected = req_n(data, "repository", |repository| req_n(object(repository)?, "pullRequest", id_object))?.flatten();
        let node = req_n(data, "node", |node| {
            let node = object(node)?;
            Ok((opt(node, "pullRequest", id_object)?, opt(node, "comments", raw_thread_comments)?))
        })?;
        let (actual, comments) = node.unwrap_or((None, None));
        Ok(GitHubReviewThreadCommentsPage {
            belongs_to_pull_request: expected.is_some() && expected == actual,
            comments: comments
                .iter()
                .flat_map(|comments| comments.nodes.iter())
                .map(|comment| to_thread_comment(comment, viewer.as_deref()))
                .collect(),
            next_cursor: next_cursor_of(comments.as_ref().and_then(|comments| comments.page_info.as_ref())),
        })
    })
}

/// `decodeBaseComparisonJson`: how far the branch trails its base; `behind_by` is `None` where
/// the head could not be compared (a fork whose repository is gone).
pub fn decode_base_comparison_json(raw: &str) -> Result<GitHubBaseComparison, DecodeFailure> {
    let value = parse_json(raw)?;
    let pull_request = req(object(&value)?, "data", |data| {
        req_n(object(data)?, "repository", |repository| {
            req_n(object(repository)?, "pullRequest", |pull_request| {
                let map = object(pull_request)?;
                let viewer_can_update_branch = opt_n(map, "viewerCanUpdateBranch", boolean)?;
                let behind_by = opt_n(map, "baseRef", |base_ref| opt_n(object(base_ref)?, "compare", behind_by))?.flatten();
                Ok((viewer_can_update_branch, behind_by))
            })
        })
    })?
    .flatten();
    let (viewer_can_update_branch, behind_by) = pull_request.unwrap_or((None, None));
    Ok(GitHubBaseComparison {
        behind_by: behind_by.filter(|behind_by| *behind_by >= 0.0).map(JsNumber),
        viewer_can_update: viewer_can_update_branch == Some(true),
    })
}

/// An insertion-ordered map, as the TS `Map` the candidate lists are gathered in.
struct OrderedMap<V> {
    entries: Vec<(String, V)>,
}

impl<V> OrderedMap<V> {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    fn has(&self, key: &str) -> bool {
        self.entries.iter().any(|(existing, _)| existing == key)
    }

    /// `Map.set`: a key already there keeps its place and takes the new value.
    fn set(&mut self, key: String, value: V) {
        match self.entries.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = value,
            None => self.entries.push((key, value)),
        }
    }

    fn into_values(self) -> Vec<V> {
        self.entries.into_iter().map(|(_, value)| value).collect()
    }
}

/// `decodeReviewerCandidatesJson`: whoever is already asked (people and teams) leads, then the
/// assignable users; the author is left out, since GitHub refuses a request to them.
pub fn decode_reviewer_candidates_json(raw: &str) -> Result<PullRequestReviewerCandidateList, DecodeFailure> {
    let value = parse_json(raw)?;
    at_path(&value, &["data", "repository"], &|repository| {
        let repository = object(repository)?;
        let (assignable_page_info, assignable) = req(repository, "assignableUsers", |users| {
            let users = object(users)?;
            Ok((
                opt(users, "pageInfo", raw_page_info)?,
                req(users, "nodes", |nodes| array(nodes, |node| null_or(node, raw_actor)))?,
            ))
        })?;
        let pull_request = req_n(repository, "pullRequest", |pull_request| {
            let map = object(pull_request)?;
            let author = opt_n(map, "author", raw_actor)?;
            let requests = opt_n(map, "reviewRequests", |requests| {
                req(object(requests)?, "nodes", |nodes| {
                    array(nodes, |node| opt_n(object(node)?, "requestedReviewer", raw_requested_reviewer))
                })
            })?;
            Ok((author, requests))
        })?;
        let (author, requests) = pull_request.unwrap_or((None, None));
        let author = trimmed(author.as_ref().and_then(|author| author.login.as_deref()));
        let mut candidates = OrderedMap::new();
        for requested in requests.iter().flatten() {
            let slug = trimmed(requested.as_ref().and_then(|reviewer| reviewer.slug.as_deref()));
            let Some(id) = slug
                .clone()
                .or_else(|| trimmed(requested.as_ref().and_then(|reviewer| reviewer.actor.login.as_deref())))
            else {
                continue;
            };
            let kind = if slug.is_none() {
                PullRequestReviewerKind::User
            } else {
                PullRequestReviewerKind::Team
            };
            candidates.set(
                format!("{} {id}", kind.as_str()),
                PullRequestReviewerCandidate {
                    is_bot: None,
                    login: id.clone(),
                    name: trimmed(requested.as_ref().and_then(|reviewer| reviewer.actor.name.as_deref())),
                    avatar_url: trimmed(requested.as_ref().and_then(|reviewer| reviewer.actor.avatar_url.as_deref())),
                    id,
                    kind,
                    is_requested: true,
                },
            );
        }
        for node in &assignable {
            let Some(login) = trimmed(node.as_ref().and_then(|node| node.login.as_deref())) else {
                continue;
            };
            let key = format!("user {login}");
            if author.as_deref() == Some(login.as_str()) || candidates.has(&key) {
                continue;
            }
            candidates.set(
                key,
                PullRequestReviewerCandidate {
                    is_bot: None,
                    login: login.clone(),
                    name: trimmed(node.as_ref().and_then(|node| node.name.as_deref())),
                    avatar_url: trimmed(node.as_ref().and_then(|node| node.avatar_url.as_deref())),
                    id: login,
                    kind: PullRequestReviewerKind::User,
                    is_requested: false,
                },
            );
        }
        Ok(PullRequestReviewerCandidateList {
            candidates: candidates.into_values(),
            truncated: assignable_page_info.and_then(|page_info| page_info.has_next_page) == Some(true),
        })
    })
}

/// `decodeLabelCandidatesJson`: the repository's labels with the applied ones marked; an applied
/// label the repository no longer defines leads, so it can still be taken off.
pub fn decode_label_candidates_json(raw: &str) -> Result<PullRequestLabelCandidateList, DecodeFailure> {
    let value = parse_json(raw)?;
    at_path(&value, &["data", "repository"], &|repository| {
        let repository = object(repository)?;
        let labels = opt_n(repository, "labels", |labels| {
            let labels = object(labels)?;
            let page_info = opt(labels, "pageInfo", raw_page_info)?;
            let nodes = req(labels, "nodes", |nodes| {
                array(nodes, |node| {
                    null_or(node, |node| {
                        let map = object(node)?;
                        Ok((raw_label(node)?, opt_n(map, "description", string)?))
                    })
                })
            })?;
            Ok((page_info, nodes))
        })?;
        let applied_nodes = req_n(repository, "pullRequest", |pull_request| {
            Ok(opt_n(object(pull_request)?, "labels", |labels| {
                req(object(labels)?, "nodes", |nodes| array(nodes, |node| null_or(node, raw_label)))
            })?
            .unwrap_or_default())
        })?
        .unwrap_or_default();
        let mut applied: Vec<String> = Vec::new();
        for name in applied_nodes
            .iter()
            .filter_map(|label| trimmed(label.as_ref().map(|label| label.name.as_str())))
        {
            if !applied.contains(&name) {
                applied.push(name);
            }
        }
        let (page_info, nodes) = labels.unwrap_or((None, Vec::new()));
        let mut candidates = OrderedMap::new();
        for (label, description) in nodes.iter().flatten() {
            let Some(name) = trimmed(Some(&label.name)) else { continue };
            candidates.set(
                name.clone(),
                PullRequestLabelCandidate {
                    is_applied: applied.contains(&name),
                    color: trimmed(label.color.as_deref()),
                    description: trimmed(description.as_deref()),
                    name,
                },
            );
        }
        let missing: Vec<PullRequestLabelCandidate> = applied
            .iter()
            .filter(|name| !candidates.has(name))
            .map(|name| PullRequestLabelCandidate {
                name: name.clone(),
                color: None,
                description: None,
                is_applied: true,
            })
            .collect();
        Ok(PullRequestLabelCandidateList {
            candidates: missing.into_iter().chain(candidates.into_values()).collect(),
            truncated: page_info.and_then(|page_info| page_info.has_next_page) == Some(true),
        })
    })
}

/// `decodeViewerPermissionsJson`: the repository's role and merge settings with the pull
/// request's own viewer fields (a pull request the viewer cannot see grants update, not authorship).
pub fn decode_viewer_permissions_json(raw: &str) -> Result<GitHubViewerRepositoryAccess, DecodeFailure> {
    let value = parse_json(raw)?;
    at_path(&value, &["data", "repository"], &|repository| {
        let repository = object(repository)?;
        let access = repository_access_fields(repository)?;
        let viewer = req_n(repository, "pullRequest", |pull_request| viewer_fields(object(pull_request)?))?;
        Ok(viewer_repository_access(&access, viewer))
    })
}

/// `ESCAPE_BY_CHARACTER` of `@t3tools/shared/gitPatchPath`.
fn git_escape(character: char) -> Option<&'static str> {
    Some(match character {
        '"' => "\\\"",
        '\\' => "\\\\",
        '\u{7}' => "\\a",
        '\u{8}' => "\\b",
        '\t' => "\\t",
        '\n' => "\\n",
        '\u{b}' => "\\v",
        '\u{c}' => "\\f",
        '\r' => "\\r",
        _ => return None,
    })
}

/// `quoteGitPatchPath` (`@t3tools/shared/gitPatchPath`): a name as a patch header carries it,
/// itself where unambiguous and git's C-quoted form where it holds a quote, a backslash or a
/// control character. Pass the header side's `a/`/`b/` in with the name.
pub fn quote_git_patch_path(path: &str) -> String {
    let mut body = String::with_capacity(path.len());
    let mut quoting = false;
    for character in path.chars() {
        if let Some(escape) = git_escape(character) {
            body.push_str(escape);
            quoting = true;
            continue;
        }
        let code = u32::from(character);
        if code < 0x20 || code == 0x7f {
            body.push_str(&format!("\\{code:03o}"));
            quoting = true;
            continue;
        }
        body.push(character);
    }
    if quoting {
        format!("\"{body}\"")
    } else {
        path.to_owned()
    }
}

/// `decodePullRequestFilesJson`: one page of the REST files API as the unified patch every diff
/// viewer expects (the API gives hunks without `diff --git` headers). A file with no hunks is
/// still listed; only one that changed lines (binary, too large) makes the patch incomplete.
pub fn decode_pull_request_files_json(raw: &str) -> Result<GitHubPullRequestFilesPatch, DecodeFailure> {
    let entries = array(&parse_json(raw)?, unknown)?;
    let mut sections = String::new();
    let mut omitted_file_stats = Vec::new();
    let mut truncated = false;
    for entry in &entries {
        let Ok(file) = raw_pull_request_file(entry) else { continue };
        let hunks = file.patch.as_deref().unwrap_or_default();
        let status = file.status.as_deref().map(|status| js_trim(status).to_lowercase());
        let status = status.as_deref();
        if hunks.is_empty() {
            let additions = file.additions.unwrap_or(0);
            let deletions = file.deletions.unwrap_or(0);
            if additions + deletions > 0 {
                truncated = true;
                #[allow(clippy::cast_precision_loss)]
                omitted_file_stats.push(PullRequestOmittedFileStat {
                    path: file.filename.clone(),
                    additions: JsNumber(additions as f64),
                    deletions: JsNumber(deletions as f64),
                });
            }
        }
        // A rename counts its hunks against the old path, which is the only place it is named.
        let old_path = match (status, file.previous_filename.as_deref()) {
            (Some("renamed"), Some(previous)) if !previous.is_empty() => previous,
            _ => file.filename.as_str(),
        };
        let mut header = vec![format!(
            "diff --git {} {}",
            quote_git_patch_path(&format!("a/{old_path}")),
            quote_git_patch_path(&format!("b/{}", file.filename))
        )];
        // The files API reports no file mode, so the ordinary one stands in.
        if status == Some("added") {
            header.push("new file mode 100644".into());
        }
        if status == Some("removed") {
            header.push("deleted file mode 100644".into());
        }
        if status == Some("renamed") {
            header.push(format!("rename from {}", quote_git_patch_path(old_path)));
            header.push(format!("rename to {}", quote_git_patch_path(&file.filename)));
        }
        header.push(format!(
            "--- {}",
            if status == Some("added") {
                "/dev/null".to_owned()
            } else {
                quote_git_patch_path(&format!("a/{old_path}"))
            }
        ));
        header.push(format!(
            "+++ {}",
            if status == Some("removed") {
                "/dev/null".to_owned()
            } else {
                quote_git_patch_path(&format!("b/{}", file.filename))
            }
        ));
        sections.push_str(&header.join("\n"));
        sections.push('\n');
        if !hunks.is_empty() {
            sections.push_str(hunks);
            if !hunks.ends_with('\n') {
                sections.push('\n');
            }
        }
    }
    Ok(GitHubPullRequestFilesPatch {
        patch: sections,
        truncated,
        raw_count: entries.len(),
        omitted_file_stats,
    })
}

/// `toFileViewedState`: anything this host does not name is unread.
fn to_file_viewed_state(raw: &str) -> PullRequestFileViewedState {
    match js_trim(raw).to_uppercase().as_str() {
        "VIEWED" => PullRequestFileViewedState::Viewed,
        "DISMISSED" => PullRequestFileViewedState::Dismissed,
        _ => PullRequestFileViewedState::Unviewed,
    }
}

/// `decodePullRequestFilesViewedJson`: each file's viewed state and where the next page carries
/// on; empty for a pull request the host has nothing to say about.
pub fn decode_pull_request_files_viewed_json(raw: &str) -> Result<GitHubPullRequestFilesViewedPage, DecodeFailure> {
    let value = parse_json(raw)?;
    let files = req(object(&value)?, "data", |data| {
        req_n(object(data)?, "repository", |repository| {
            req_n(object(repository)?, "pullRequest", |pull_request| {
                req(object(pull_request)?, "files", |files| {
                    let files = object(files)?;
                    let (has_next_page, end_cursor) = req(files, "pageInfo", |page| {
                        let page = object(page)?;
                        Ok((req(page, "hasNextPage", boolean)?, req_n(page, "endCursor", string)?))
                    })?;
                    let nodes = req_n(files, "nodes", |nodes| {
                        array(nodes, |node| {
                            null_or(node, |node| {
                                let node = object(node)?;
                                Ok((req(node, "path", string)?, req(node, "viewerViewedState", string)?))
                            })
                        })
                    })?;
                    Ok((has_next_page, end_cursor, nodes))
                })
            })
        })
    })?
    .flatten();
    let Some((has_next_page, end_cursor, nodes)) = files else {
        return Ok(GitHubPullRequestFilesViewedPage {
            files: Vec::new(),
            next_cursor: None,
        });
    };
    Ok(GitHubPullRequestFilesViewedPage {
        files: nodes
            .into_iter()
            .flatten()
            .flatten()
            .filter(|(path, _)| !path.is_empty())
            .map(|(path, state)| PullRequestFileViewed {
                state: to_file_viewed_state(&state),
                path,
            })
            .collect(),
        next_cursor: if has_next_page { end_cursor } else { None },
    })
}

/// `RawStackSchema.id`: `Schema.Union([Schema.Int, Schema.String])`, as `String(id)`.
fn int_or_string(value: &Value) -> Decoded<String> {
    match int(value) {
        Ok(id) => Ok(id.to_string()),
        Err(_) => string(value).map_err(|_| DecodeFailure::invalid_value()),
    }
}

/// `decodePullRequestStacksJson`: the first stack of a `?pull_request=` listing (a pull request
/// is in at most one), or `None` for an empty one. `base` is the ref object the preview sends
/// today or the bare branch name it started out with; `merged_at` outranks `state`.
pub fn decode_pull_request_stacks_json(raw: &str) -> Result<Option<GitHubPullRequestStack>, DecodeFailure> {
    let stacks = array(&parse_json(raw)?, |stack| {
        let map = object(stack)?;
        let id = opt_n(map, "id", int_or_string)?;
        let number = req(map, "number", int)?;
        let node_id = opt_n(map, "node_id", string)?;
        let url = req(map, "url", string)?;
        let html_url = opt_n(map, "html_url", string)?;
        let base = req(map, "base", |base| match base {
            Value::String(base) => Ok(base.clone()),
            Value::Object(base) => req(base, "ref", string),
            _ => Err(DecodeFailure::invalid_value()),
        })?;
        let layers = req(map, "pull_requests", |pull_requests| {
            array(pull_requests, |pull_request| {
                let map = object(pull_request)?;
                let title = opt(map, "title", string)?;
                let is_draft = opt(map, "draft", boolean)?;
                let number = req(map, "number", int)?;
                let (head_branch, head_sha) = req(map, "head", |head| {
                    let head = object(head)?;
                    Ok((req(head, "ref", string)?, opt(head, "sha", string)?))
                })?;
                let state = opt_n(map, "state", string)?;
                let merged_at = opt_n(map, "merged_at", string)?;
                Ok(GitHubPullRequestStackLayer {
                    title,
                    is_draft,
                    head_sha,
                    number,
                    head_branch,
                    state: to_state(state.as_deref(), merged_at.as_deref()),
                })
            })
        })?;
        Ok(GitHubPullRequestStack {
            id: id.or_else(|| trimmed(node_id.as_deref())).unwrap_or_else(|| number.to_string()),
            number,
            // The page a person opens where the preview reports one; the API URL otherwise.
            url: trimmed(html_url.as_deref()).unwrap_or(url),
            base,
            layers,
        })
    })?;
    Ok(stacks.into_iter().next())
}
