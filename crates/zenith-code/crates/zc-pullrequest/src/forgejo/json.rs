//! `pullRequest/forgejoPullRequestJson.ts`: the Forgejo/Gitea REST shapes (decoded with the TS
//! `Schema.Struct` rules, see [`crate::gitlab::util`]) and their neutral pull request forms.

use serde_json::{Map, Value};
use zc_contracts::{
    DateTimeUtc, PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestComment, PullRequestCommentKind, PullRequestCommit,
    PullRequestDiffSide, PullRequestLabel, PullRequestMergeability, PullRequestReaction, PullRequestReactionContent, PullRequestReviewThread, PullRequestState,
};

use crate::checks::{dedupe_checks, CheckEntry};
use crate::gitlab::util::{
    array, boolean, int, nullable_string, nullable_struct, object, opt_array, opt_bool, opt_int, opt_string, opt_struct, required_struct, string, Decoded,
    Mismatch,
};
use crate::provider::ProviderChangeRequest;

/// `ForgejoUser`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoUser {
    pub login: String,
    pub full_name: Option<String>,
    pub avatar_url: Option<String>,
}

pub fn decode_user(object: &Map<String, Value>) -> Decoded<ForgejoUser> {
    Ok(ForgejoUser {
        login: string(object, "login")?,
        full_name: opt_string(object, "full_name", true)?,
        avatar_url: opt_string(object, "avatar_url", true)?,
    })
}

pub fn decode_user_item(value: &Value) -> Decoded<ForgejoUser> {
    decode_user(object(value)?)
}

fn nullable_user(object: &Map<String, Value>, key: &str) -> Decoded<Option<ForgejoUser>> {
    nullable_struct(object, key, decode_user)
}

/// `ForgejoLabel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoLabel {
    pub id: i64,
    pub name: String,
    pub color: Option<String>,
    pub description: Option<String>,
}

pub fn decode_label(value: &Value) -> Decoded<ForgejoLabel> {
    let label = object(value)?;
    Ok(ForgejoLabel {
        id: int(label, "id")?,
        name: string(label, "name")?,
        color: opt_string(label, "color", true)?,
        description: opt_string(label, "description", true)?,
    })
}

/// `ForgejoRepository`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoRepository {
    pub full_name: String,
    /// `permissions.push` (the struct is optional, its fields are not).
    pub permissions_push: Option<bool>,
    pub archived: Option<bool>,
    pub allow_merge_commits: Option<bool>,
    pub allow_squash_merge: Option<bool>,
    pub allow_rebase: Option<bool>,
    pub allow_rebase_update: Option<bool>,
}

pub fn decode_repository(repository: &Map<String, Value>) -> Decoded<ForgejoRepository> {
    Ok(ForgejoRepository {
        full_name: string(repository, "full_name")?,
        permissions_push: opt_struct(repository, "permissions", false, |permissions| {
            boolean(permissions, "admin")?;
            boolean(permissions, "push")
        })?,
        archived: opt_bool(repository, "archived", false)?,
        allow_merge_commits: opt_bool(repository, "allow_merge_commits", false)?,
        allow_squash_merge: opt_bool(repository, "allow_squash_merge", false)?,
        allow_rebase: opt_bool(repository, "allow_rebase", false)?,
        allow_rebase_update: opt_bool(repository, "allow_rebase_update", false)?,
    })
}

/// `Branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoBranch {
    pub reference: String,
    pub sha: String,
    pub repo: Option<ForgejoRepository>,
}

fn decode_branch(branch: &Map<String, Value>) -> Decoded<ForgejoBranch> {
    Ok(ForgejoBranch {
        reference: string(branch, "ref")?,
        sha: string(branch, "sha")?,
        repo: nullable_struct(branch, "repo", decode_repository)?,
    })
}

/// `ForgejoPullRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoPullRequest {
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub html_url: String,
    pub user: Option<ForgejoUser>,
    pub state: String,
    pub draft: Option<bool>,
    pub merged: bool,
    pub mergeable: Option<bool>,
    pub is_locked: Option<bool>,
    pub head: ForgejoBranch,
    pub base: ForgejoBranch,
    pub merge_base: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
    pub labels: Option<Vec<ForgejoLabel>>,
    pub requested_reviewers: Option<Vec<ForgejoUser>>,
}

pub fn decode_pull_request(pr: &Map<String, Value>) -> Decoded<ForgejoPullRequest> {
    opt_int(pr, "comments", false)?;
    Ok(ForgejoPullRequest {
        number: int(pr, "number")?,
        title: string(pr, "title")?,
        body: nullable_string(pr, "body")?,
        html_url: string(pr, "html_url")?,
        user: nullable_user(pr, "user")?,
        state: string(pr, "state")?,
        draft: opt_bool(pr, "draft", false)?,
        merged: boolean(pr, "merged")?,
        mergeable: opt_bool(pr, "mergeable", false)?,
        is_locked: opt_bool(pr, "is_locked", false)?,
        head: required_struct(pr, "head", decode_branch)?,
        base: required_struct(pr, "base", decode_branch)?,
        merge_base: opt_string(pr, "merge_base", false)?,
        created_at: string(pr, "created_at")?,
        updated_at: string(pr, "updated_at")?,
        closed_at: nullable_string(pr, "closed_at")?,
        merged_at: nullable_string(pr, "merged_at")?,
        additions: opt_int(pr, "additions", true)?,
        deletions: opt_int(pr, "deletions", true)?,
        changed_files: opt_int(pr, "changed_files", true)?,
        labels: match pr.get("labels") {
            None => return Err(Mismatch),
            Some(Value::Null) => None,
            Some(labels) => Some(array(labels, decode_label)?),
        },
        requested_reviewers: opt_array(pr, "requested_reviewers", true, decode_user_item)?,
    })
}

/// `ForgejoComment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoComment {
    pub id: i64,
    pub body: String,
    pub user: Option<ForgejoUser>,
    pub created_at: String,
    pub html_url: Option<String>,
}

fn decode_comment_fields(comment: &Map<String, Value>) -> Decoded<ForgejoComment> {
    Ok(ForgejoComment {
        id: int(comment, "id")?,
        body: string(comment, "body")?,
        user: nullable_user(comment, "user")?,
        created_at: string(comment, "created_at")?,
        html_url: opt_string(comment, "html_url", false)?,
    })
}

pub fn decode_comment(value: &Value) -> Decoded<ForgejoComment> {
    decode_comment_fields(object(value)?)
}

/// `ForgejoReview`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoReview {
    pub id: i64,
    pub body: String,
    pub user: Option<ForgejoUser>,
    pub state: String,
    pub submitted_at: String,
    pub html_url: Option<String>,
    pub comments_count: i64,
}

pub fn decode_review_fields(review: &Map<String, Value>) -> Decoded<ForgejoReview> {
    Ok(ForgejoReview {
        id: int(review, "id")?,
        body: string(review, "body")?,
        user: nullable_user(review, "user")?,
        state: string(review, "state")?,
        submitted_at: string(review, "submitted_at")?,
        html_url: opt_string(review, "html_url", false)?,
        comments_count: int(review, "comments_count")?,
    })
}

pub fn decode_review(value: &Value) -> Decoded<ForgejoReview> {
    decode_review_fields(object(value)?)
}

/// `ForgejoReviewComment`: a comment with its place in the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoReviewComment {
    pub comment: ForgejoComment,
    pub path: String,
    pub position: i64,
    pub original_position: i64,
    pub resolver: Option<ForgejoUser>,
}

pub fn decode_review_comment(value: &Value) -> Decoded<ForgejoReviewComment> {
    let comment = object(value)?;
    string(comment, "commit_id")?;
    string(comment, "original_commit_id")?;
    Ok(ForgejoReviewComment {
        comment: decode_comment_fields(comment)?,
        path: string(comment, "path")?,
        position: int(comment, "position")?,
        original_position: int(comment, "original_position")?,
        resolver: nullable_user(comment, "resolver")?,
    })
}

/// `ForgejoCommit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoCommit {
    pub sha: String,
    pub author: Option<ForgejoUser>,
    pub message: String,
    pub committer_date: String,
    pub parents: Vec<String>,
    pub stats: Option<(i64, i64)>,
}

pub fn decode_commit_fields(commit: &Map<String, Value>) -> Decoded<ForgejoCommit> {
    let (message, committer_date) = required_struct(commit, "commit", |inner| {
        Ok((
            string(inner, "message")?,
            required_struct(inner, "committer", |committer| string(committer, "date"))?,
        ))
    })?;
    Ok(ForgejoCommit {
        sha: string(commit, "sha")?,
        author: nullable_user(commit, "author")?,
        message,
        committer_date,
        parents: array(commit.get("parents").ok_or(Mismatch)?, |parent| string(object(parent)?, "sha"))?,
        stats: opt_struct(commit, "stats", true, |stats| Ok((int(stats, "additions")?, int(stats, "deletions")?)))?,
    })
}

pub fn decode_commit(value: &Value) -> Decoded<ForgejoCommit> {
    decode_commit_fields(object(value)?)
}

/// `ForgejoStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoStatus {
    pub context: String,
    pub status: String,
    pub description: Option<String>,
    pub target_url: Option<String>,
    pub updated_at: String,
}

pub fn decode_status(value: &Value) -> Decoded<ForgejoStatus> {
    let status = object(value)?;
    Ok(ForgejoStatus {
        context: string(status, "context")?,
        status: string(status, "status")?,
        description: nullable_string(status, "description")?,
        target_url: nullable_string(status, "target_url")?,
        updated_at: string(status, "updated_at")?,
    })
}

/// `ForgejoReaction`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoReaction {
    pub content: String,
    pub user: Option<ForgejoUser>,
}

pub fn decode_reaction(value: &Value) -> Decoded<ForgejoReaction> {
    let reaction = object(value)?;
    Ok(ForgejoReaction {
        content: string(reaction, "content")?,
        user: nullable_user(reaction, "user")?,
    })
}

/// `forgejoActor`: `None` without a login; an empty name or avatar reads as none.
pub fn forgejo_actor(user: Option<&ForgejoUser>) -> Option<PullRequestActor> {
    let user = user.filter(|user| !user.login.is_empty())?;
    Some(PullRequestActor {
        is_bot: None,
        login: user.login.clone(),
        name: user.full_name.clone().filter(|name| !name.is_empty()),
        avatar_url: user.avatar_url.clone().filter(|url| !url.is_empty()),
    })
}

/// `toIsoUtc`: the instant as `toISOString` writes it, or the text as it came when it is no date.
pub fn to_iso_utc(value: &str) -> String {
    DateTimeUtc::parse(value).map_or_else(|_| value.to_owned(), DateTimeUtc::to_iso_string)
}

/// `/^(?:\[WIP\]|WIP:)/i`.
fn is_wip_title(title: &str) -> bool {
    let lower = title.get(..5).map(str::to_ascii_lowercase);
    lower.as_deref() == Some("[wip]") || title.get(..4).map(str::to_ascii_lowercase).as_deref() == Some("wip:")
}

/// `forgejoChangeRequest`.
pub fn forgejo_change_request(pr: &ForgejoPullRequest) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number: pr.number,
        title: pr.title.clone(),
        url: pr.html_url.clone(),
        author: forgejo_actor(pr.user.as_ref()),
        head_branch: pr.head.reference.clone(),
        head_repository_name_with_owner: Some(pr.head.repo.as_ref().map(|repo| repo.full_name.clone())),
        base_branch: pr.base.reference.clone(),
        state: if pr.merged {
            PullRequestState::Merged
        } else if pr.state == "closed" {
            PullRequestState::Closed
        } else {
            PullRequestState::Open
        },
        is_draft: pr.draft.unwrap_or_else(|| is_wip_title(&pr.title)),
        mergeability: match pr.mergeable {
            None => PullRequestMergeability::Unknown,
            Some(true) => PullRequestMergeability::Mergeable,
            Some(false) => PullRequestMergeability::Conflicting,
        },
        additions: pr.additions.unwrap_or(0),
        deletions: pr.deletions.unwrap_or(0),
        created_at: to_iso_utc(&pr.created_at),
        closed_at: Some(pr.closed_at.as_deref().map(to_iso_utc)),
        merged_at: Some(pr.merged_at.as_deref().map(to_iso_utc)),
        updated_at: to_iso_utc(&pr.updated_at),
        review_request_logins: pr
            .requested_reviewers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|user| user.login.clone())
            .collect(),
        labels: pr
            .labels
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|label| PullRequestLabel {
                name: label.name.clone(),
                color: label.color.clone(),
            })
            .collect(),
        review_decision: None,
        checks_state: None,
    }
}

/// `forgejoComment`.
pub fn forgejo_comment(comment: &ForgejoComment) -> PullRequestComment {
    PullRequestComment {
        id: comment.id.to_string(),
        kind: PullRequestCommentKind::IssueComment,
        author: forgejo_actor(comment.user.as_ref()),
        body: comment.body.clone(),
        created_at: to_iso_utc(&comment.created_at),
        url: comment.html_url.clone().filter(|url| !url.is_empty()),
        path: None,
        review_state: None,
        reactions: None,
    }
}

/// `forgejoReview`: a review as a timeline entry, its state in GitHub's spelling.
pub fn forgejo_review(review: &ForgejoReview) -> PullRequestComment {
    PullRequestComment {
        id: format!("review:{}", review.id),
        kind: PullRequestCommentKind::Review,
        author: forgejo_actor(review.user.as_ref()),
        body: review.body.clone(),
        created_at: to_iso_utc(&review.submitted_at),
        url: review.html_url.clone().filter(|url| !url.is_empty()),
        path: None,
        review_state: Some(match review.state.as_str() {
            "REQUEST_CHANGES" => "CHANGES_REQUESTED".to_owned(),
            "COMMENT" => "COMMENTED".to_owned(),
            other => other.to_owned(),
        }),
        reactions: None,
    }
}

/// `forgejoReviewThread`: one inline comment as its own thread. A comment with no current
/// position but an original one sits on the old side.
pub fn forgejo_review_thread(comment: &ForgejoReviewComment) -> PullRequestReviewThread {
    let old_side = comment.position == 0 && comment.original_position > 0;
    let line = if old_side { comment.original_position } else { comment.position };
    let thread_comment = forgejo_comment(&comment.comment);
    PullRequestReviewThread {
        id: comment.comment.id.to_string(),
        path: comment.path.clone(),
        line: (line > 0).then_some(line),
        side: if old_side { PullRequestDiffSide::Left } else { PullRequestDiffSide::Right },
        is_resolved: comment.resolver.is_some(),
        is_outdated: false,
        comments: vec![zc_contracts::PullRequestThreadComment {
            id: thread_comment.id,
            author: thread_comment.author,
            body: thread_comment.body,
            created_at: thread_comment.created_at,
            url: thread_comment.url,
            reactions: None,
        }],
        comment_count: None,
        next_comments_cursor: None,
    }
}

/// `forgejoCommit`.
pub fn forgejo_commit(commit: &ForgejoCommit) -> PullRequestCommit {
    PullRequestCommit {
        oid: commit.sha.clone(),
        message_headline: commit.message.split('\n').next().unwrap_or_default().to_owned(),
        committed_date: to_iso_utc(&commit.committer_date),
        additions: commit.stats.map(|(additions, _)| additions),
        deletions: commit.stats.map(|(_, deletions)| deletions),
        authors: Some(forgejo_actor(commit.author.as_ref()).into_iter().collect()),
    }
}

/// `forgejoChecks`: commit statuses as checks, newest run of each context.
pub fn forgejo_checks(statuses: &[ForgejoStatus]) -> Vec<PullRequestCheck> {
    let entries: Vec<CheckEntry> = statuses
        .iter()
        .map(|status| CheckEntry {
            workflow_name: None,
            at: Some(status.updated_at.clone()),
            check: PullRequestCheck {
                name: if status.context.is_empty() { "check".into() } else { status.context.clone() },
                description: status.description.clone().filter(|text| !text.is_empty()),
                url: status.target_url.clone().filter(|url| !url.is_empty()),
                status: match status.status.as_str() {
                    "success" => PullRequestCheckStatus::Success,
                    "failure" | "error" => PullRequestCheckStatus::Failure,
                    _ => PullRequestCheckStatus::Pending,
                },
            },
        })
        .collect();
    dedupe_checks(&entries)
}

/// `FORGEJO_REACTIONS`: the reaction names Forgejo stores for the eight the contract carries.
pub fn forgejo_reaction_name(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "+1",
        PullRequestReactionContent::ThumbsDown => "-1",
        PullRequestReactionContent::Laugh => "laugh",
        PullRequestReactionContent::Hooray => "hooray",
        PullRequestReactionContent::Confused => "confused",
        PullRequestReactionContent::Heart => "heart",
        PullRequestReactionContent::Rocket => "rocket",
        PullRequestReactionContent::Eyes => "eyes",
    }
}

/// `forgejoReactions`: grouped per content, the viewer counted but not named.
pub fn forgejo_reactions(reactions: &[ForgejoReaction], viewer: &str) -> Vec<PullRequestReaction> {
    PullRequestReactionContent::ALL
        .iter()
        .filter_map(|&content| {
            let emoji = forgejo_reaction_name(content);
            let matching: Vec<&ForgejoReaction> = reactions.iter().filter(|reaction| reaction.content == emoji).collect();
            if matching.is_empty() {
                return None;
            }
            let login = |reaction: &&ForgejoReaction| reaction.user.as_ref().map(|user| user.login.clone());
            Some(PullRequestReaction {
                content,
                count: matching.len() as i64,
                actors: matching.iter().filter_map(login).filter(|login| !login.is_empty() && login != viewer).collect(),
                viewer_has_reacted: matching.iter().filter_map(login).any(|login| login == viewer),
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "json_tests.rs"]
mod tests;
