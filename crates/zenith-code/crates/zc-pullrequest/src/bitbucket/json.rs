//! `pullRequest/bitbucketPullRequestJson.ts`: Bitbucket Cloud's REST payloads, decoded the way
//! the TS `Schema` decoders do (excess keys ignored, `optional` keys may be absent, `NullOr` ones
//! may also be `null`, `TrimmedNonEmptyString` trims) and normalized to the neutral types.
//!
//! Bitbucket's enums are decoded as plain strings and normalized here, in the same tolerant style
//! as the GitHub and GitLab decoders: a new pull request state or build status must not fail a
//! whole payload. Malformed entries of a page are skipped rather than failing the page.

use std::collections::HashMap;

use serde_json::{Map, Value};
use zc_contracts::{
    DateTimeUtc, PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestComment, PullRequestCommentKind, PullRequestCommit,
    PullRequestDiffSide, PullRequestMergeability, PullRequestReviewThread, PullRequestReviewerCandidate, PullRequestReviewerKind, PullRequestState,
    PullRequestThreadComment,
};
use zc_sourcecontrol::github::cli::SchemaDecodeError;
use zc_sourcecontrol::util::{js_trim, safe_int, trimmed_non_empty};

use crate::checks::{dedupe_checks, CheckEntry};

/// A payload that does not match its schema (the TS `Cause<SchemaError>`).
pub type DecodeFailure = SchemaDecodeError;

type Decoded<T> = Result<T, Mismatch>;

/// One schema mismatch, named by the field it was found at.
#[derive(Debug, Clone, Copy)]
struct Mismatch(&'static str);

impl Mismatch {
    fn failure(self) -> DecodeFailure {
        SchemaDecodeError(format!("Invalid Bitbucket response at {}", self.0))
    }
}

fn parse(raw: &str) -> Result<Value, DecodeFailure> {
    serde_json::from_str(raw).map_err(|error| SchemaDecodeError(error.to_string()))
}

fn object<'a>(value: &'a Value, at: &'static str) -> Decoded<&'a Map<String, Value>> {
    value.as_object().ok_or(Mismatch(at))
}

/// `Schema.optional(X)` (`nullable` adds `NullOr`): absent, and `null` where allowed, are `None`.
fn optional<'a>(map: &'a Map<String, Value>, key: &'static str, nullable: bool) -> Decoded<Option<&'a Value>> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::Null) if nullable => Ok(None),
        Some(Value::Null) => Err(Mismatch(key)),
        Some(value) => Ok(Some(value)),
    }
}

fn required<'a>(map: &'a Map<String, Value>, key: &'static str) -> Decoded<&'a Value> {
    map.get(key).ok_or(Mismatch(key))
}

fn string(map: &Map<String, Value>, key: &'static str) -> Decoded<String> {
    required(map, key)?.as_str().map(str::to_owned).ok_or(Mismatch(key))
}

fn optional_string(map: &Map<String, Value>, key: &'static str, nullable: bool) -> Decoded<Option<String>> {
    optional(map, key, nullable)?
        .map(|value| value.as_str().map(str::to_owned).ok_or(Mismatch(key)))
        .transpose()
}

fn int(value: &Value, at: &'static str) -> Decoded<i64> {
    safe_int(value).ok_or(Mismatch(at))
}

fn optional_int(map: &Map<String, Value>, key: &'static str) -> Decoded<Option<i64>> {
    optional(map, key, true)?.map(|value| int(value, key)).transpose()
}

fn optional_bool(map: &Map<String, Value>, key: &'static str) -> Decoded<Option<bool>> {
    optional(map, key, false)?.map(|value| value.as_bool().ok_or(Mismatch(key))).transpose()
}

/// `TrimmedNonEmptyString`.
fn trimmed_string(map: &Map<String, Value>, key: &'static str) -> Decoded<String> {
    trimmed_non_empty(required(map, key)?).ok_or(Mismatch(key))
}

fn optional_object<'a>(map: &'a Map<String, Value>, key: &'static str) -> Decoded<Option<&'a Map<String, Value>>> {
    optional(map, key, true)?.map(|value| object(value, key)).transpose()
}

/// `trimmed(value)`: the trimmed text, `None` when absent or blank.
fn trimmed(value: Option<&str>) -> Option<String> {
    let text = js_trim(value.unwrap_or_default());
    (!text.is_empty()).then(|| text.to_owned())
}

/// `toIsoUtc`: Bitbucket stamps times as `+00:00` with microseconds. The page sorts change
/// requests from every host against each other as plain strings, so they are normalized to the
/// same `Z` form the other hosts already use; a value that is not a date passes through.
pub fn to_iso_utc(value: &str) -> String {
    DateTimeUtc::parse(value).map_or_else(|_| value.to_owned(), DateTimeUtc::to_iso_string)
}

/// `RawUserSchema`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawUser {
    /// How Bitbucket addresses an account when a reviewer set is written; braced, and sent back
    /// exactly as it arrived.
    pub uuid: Option<String>,
    /// Absent on an app account, which is why `display_name` has to stand in for it.
    pub nickname: Option<String>,
    pub display_name: Option<String>,
    pub avatar_href: Option<String>,
}

fn decode_user(value: &Value, at: &'static str) -> Decoded<RawUser> {
    let map = object(value, at)?;
    let avatar_href = match optional_object(map, "links")? {
        None => None,
        Some(links) => match optional_object(links, "avatar")? {
            None => None,
            Some(avatar) => optional_string(avatar, "href", false)?,
        },
    };
    Ok(RawUser {
        uuid: optional_string(map, "uuid", true)?,
        nickname: optional_string(map, "nickname", true)?,
        display_name: optional_string(map, "display_name", true)?,
        avatar_href,
    })
}

fn optional_user(map: &Map<String, Value>, key: &'static str) -> Decoded<Option<RawUser>> {
    optional(map, key, true)?.map(|value| decode_user(value, key)).transpose()
}

/// `toActor`: an app account has no nickname, so the display name is the only handle it has.
fn to_actor(raw: Option<&RawUser>) -> Option<PullRequestActor> {
    let login = trimmed(raw.and_then(|user| user.nickname.as_deref())).or_else(|| trimmed(raw.and_then(|user| user.display_name.as_deref())))?;
    Some(PullRequestActor {
        is_bot: None,
        login,
        name: trimmed(raw.and_then(|user| user.display_name.as_deref())),
        avatar_url: trimmed(raw.and_then(|user| user.avatar_href.as_deref())),
    })
}

/// `RawBranchSchema`: required, and required to be non-empty: the wire contract will not carry a
/// change request without a branch or a link, so a row missing one is skipped.
struct RawBranch {
    name: String,
    full_name: Option<String>,
}

fn decode_branch(map: &Map<String, Value>, key: &'static str) -> Decoded<RawBranch> {
    let branch = object(required(map, key)?, key)?;
    let name = trimmed_string(object(required(branch, "branch")?, "branch")?, "name")?;
    let full_name = match optional_object(branch, "repository")? {
        None => None,
        Some(repository) => Some(trimmed_string(repository, "full_name")?),
    };
    Ok(RawBranch { name, full_name })
}

struct RawParticipant {
    user: Option<RawUser>,
    approved: Option<bool>,
    state: Option<String>,
    participated_on: Option<String>,
}

/// `RawPullRequestSchema`.
struct RawPullRequest {
    id: i64,
    title: String,
    description: Option<String>,
    state: Option<String>,
    draft: Option<bool>,
    author: Option<RawUser>,
    source: RawBranch,
    destination: RawBranch,
    created_on: String,
    updated_on: String,
    reviewers: Option<Vec<RawUser>>,
    participants: Option<Vec<RawParticipant>>,
    html_href: String,
}

fn decode_raw_pull_request(value: &Value) -> Decoded<RawPullRequest> {
    let map = object(value, "pull request")?;
    let reviewers = match optional(map, "reviewers", true)? {
        None => None,
        Some(list) => Some(
            list.as_array()
                .ok_or(Mismatch("reviewers"))?
                .iter()
                .map(|user| decode_user(user, "reviewers"))
                .collect::<Decoded<Vec<_>>>()?,
        ),
    };
    let participants = match optional(map, "participants", true)? {
        None => None,
        Some(list) => Some(
            list.as_array()
                .ok_or(Mismatch("participants"))?
                .iter()
                .map(|participant| {
                    let participant = object(participant, "participants")?;
                    optional_string(participant, "role", true)?;
                    Ok(RawParticipant {
                        user: optional_user(participant, "user")?,
                        approved: optional_bool(participant, "approved")?,
                        state: optional_string(participant, "state", true)?,
                        participated_on: optional_string(participant, "participated_on", true)?,
                    })
                })
                .collect::<Decoded<Vec<_>>>()?,
        ),
    };
    let links = object(required(map, "links")?, "links")?;
    let html_href = trimmed_string(object(required(links, "html")?, "html")?, "href")?;
    Ok(RawPullRequest {
        id: int(required(map, "id")?, "id")?,
        title: string(map, "title")?,
        description: optional_string(map, "description", true)?,
        state: optional_string(map, "state", true)?,
        draft: optional_bool(map, "draft")?,
        author: optional_user(map, "author")?,
        source: decode_branch(map, "source")?,
        destination: decode_branch(map, "destination")?,
        created_on: string(map, "created_on")?,
        updated_on: string(map, "updated_on")?,
        reviewers,
        participants,
        html_href,
    })
}

/// A pull request as the rest of this provider reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct BitbucketPullRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub head_repository_name_with_owner: Option<String>,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    /// Bitbucket reports no conflict state on a pull request, so the list leaves it unknown. The
    /// detail read asks the conflicts endpoint, which does answer.
    pub mergeability: PullRequestMergeability,
    pub created_at: String,
    pub updated_at: String,
    pub body: String,
    pub review_request_logins: Vec<String>,
    pub reviewers: Vec<PullRequestActor>,
    /// The reviewers as Bitbucket addresses them, which is what writing the set back takes.
    pub reviewer_ids: Vec<String>,
    /// Approvals and change requests, which Bitbucket keeps on its participants.
    pub reviews: Vec<PullRequestComment>,
}

fn to_state(raw: &RawPullRequest) -> PullRequestState {
    match raw.state.as_deref().map(|state| js_trim(state).to_uppercase()).as_deref() {
        Some("MERGED") => PullRequestState::Merged,
        Some("DECLINED" | "SUPERSEDED") => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

fn to_build_status(value: Option<&str>) -> PullRequestCheckStatus {
    match value.map(|value| js_trim(value).to_uppercase()).as_deref() {
        Some("SUCCESSFUL") => PullRequestCheckStatus::Success,
        Some("FAILED") => PullRequestCheckStatus::Failure,
        Some("STOPPED") => PullRequestCheckStatus::Cancelled,
        Some("INPROGRESS") => PullRequestCheckStatus::Pending,
        _ => PullRequestCheckStatus::Neutral,
    }
}

/// `toReviews`: a participant who has voted is the closest Bitbucket has to a review, so it reads
/// as one in the conversation. Participants who have only been added carry no verdict.
fn to_reviews(raw: &RawPullRequest) -> Vec<PullRequestComment> {
    raw.participants
        .iter()
        .flatten()
        .filter_map(|participant| {
            let author = to_actor(participant.user.as_ref())?;
            let voted_at = trimmed(participant.participated_on.as_deref())?;
            let review_state = trimmed(participant.state.as_deref()).or_else(|| (participant.approved == Some(true)).then(|| "approved".to_owned()))?;
            Some(PullRequestComment {
                id: format!("{}:{}", raw.id, author.login),
                kind: PullRequestCommentKind::Review,
                author: Some(author),
                body: String::new(),
                created_at: to_iso_utc(&voted_at),
                url: None,
                path: None,
                review_state: Some(review_state),
                reactions: None,
            })
        })
        .collect()
}

fn to_pull_request(raw: RawPullRequest) -> BitbucketPullRequest {
    let reviewers: Vec<PullRequestActor> = raw.reviewers.iter().flatten().filter_map(|reviewer| to_actor(Some(reviewer))).collect();
    BitbucketPullRequest {
        number: raw.id,
        title: raw.title.clone(),
        url: raw.html_href.clone(),
        author: to_actor(raw.author.as_ref()),
        head_branch: raw.source.name.clone(),
        head_repository_name_with_owner: raw.source.full_name.clone(),
        base_branch: raw.destination.name.clone(),
        state: to_state(&raw),
        is_draft: raw.draft.unwrap_or(false),
        mergeability: PullRequestMergeability::Unknown,
        created_at: to_iso_utc(&raw.created_on),
        updated_at: to_iso_utc(&raw.updated_on),
        body: raw.description.clone().unwrap_or_default(),
        review_request_logins: reviewers.iter().map(|reviewer| reviewer.login.clone()).collect(),
        reviewers,
        reviewer_ids: raw
            .reviewers
            .iter()
            .flatten()
            .filter_map(|reviewer| trimmed(reviewer.uuid.as_deref()))
            .collect(),
        reviews: to_reviews(&raw),
    }
}

/// `RawPageSchema`.
struct RawPage {
    values: Vec<Value>,
    next: Option<String>,
}

fn decode_page(raw: &str) -> Result<RawPage, DecodeFailure> {
    let value = parse(raw)?;
    let page = (|| -> Decoded<RawPage> {
        let map = object(&value, "page")?;
        let values = required(map, "values")?.as_array().ok_or(Mismatch("values"))?.clone();
        // A total count, which Bitbucket omits on some endpoints.
        optional_int(map, "size")?;
        Ok(RawPage {
            values,
            next: optional_string(map, "next", true)?,
        })
    })();
    page.map_err(Mismatch::failure)
}

/// A page of decoded items.
#[derive(Debug, Clone, PartialEq)]
pub struct BitbucketPage<A> {
    pub items: Vec<A>,
    /// The whole URL of the next page, which Bitbucket sends rather than an offset.
    pub next: Option<String>,
}

/// `decodePullRequestPageJson`: malformed entries are skipped rather than failing the page.
pub fn decode_pull_request_page_json(raw: &str) -> Result<BitbucketPage<BitbucketPullRequest>, DecodeFailure> {
    let page = decode_page(raw)?;
    Ok(BitbucketPage {
        items: page
            .values
            .iter()
            .filter_map(|entry| decode_raw_pull_request(entry).ok())
            .map(to_pull_request)
            .collect(),
        next: trimmed(page.next.as_deref()),
    })
}

/// `decodePullRequestJson`.
pub fn decode_pull_request_json(raw: &str) -> Result<BitbucketPullRequest, DecodeFailure> {
    decode_raw_pull_request(&parse(raw)?).map(to_pull_request).map_err(Mismatch::failure)
}

/// `decodeViewerJson`: the nickname, else the display name (an app account's only handle).
pub fn decode_viewer_json(raw: &str) -> Result<Option<String>, DecodeFailure> {
    let value = parse(raw)?;
    let decoded = (|| -> Decoded<Option<String>> {
        let map = object(&value, "user")?;
        let nickname = optional_string(map, "nickname", true)?;
        let display_name = optional_string(map, "display_name", true)?;
        Ok(trimmed(nickname.as_deref()).or_else(|| trimmed(display_name.as_deref())))
    })();
    decoded.map_err(Mismatch::failure)
}

/// `decodeRepositoryPermissionJson`: whether the configured credentials can write to the
/// repository, which is what merging needs. Bitbucket answers `admin`, `write` or `read`, and an
/// empty page means it named no permission at all for this account — an unknown standing, which
/// is granted rather than guessed away.
pub fn decode_repository_permission_json(raw: &str) -> Result<bool, DecodeFailure> {
    let value = parse(raw)?;
    let decoded = (|| -> Decoded<Option<String>> {
        let map = object(&value, "permissions")?;
        let Some(values) = optional(map, "values", true)? else { return Ok(None) };
        let mut permissions = Vec::new();
        for entry in values.as_array().ok_or(Mismatch("values"))? {
            permissions.push(optional_string(object(entry, "values")?, "permission", true)?);
        }
        Ok(permissions.into_iter().next().flatten())
    })();
    let permission = decoded.map_err(Mismatch::failure)?;
    Ok(match trimmed(permission.as_deref()).map(|permission| permission.to_lowercase()) {
        None => true,
        Some(permission) => permission == "admin" || permission == "write",
    })
}

/// `decodeWorkspaceMembersJson`: the workspace's members, the nearest thing Bitbucket has to "who
/// may review this" (a pull request can be sent to anyone in the workspace). Nobody is marked
/// requested here: who has been asked lives on the pull request.
pub fn decode_workspace_members_json(raw: &str) -> Result<BitbucketPage<PullRequestReviewerCandidate>, DecodeFailure> {
    let page = decode_page(raw)?;
    let items = page
        .values
        .iter()
        .filter_map(|entry| {
            let user = optional_user(object(entry, "members").ok()?, "user").ok()?;
            let uuid = trimmed(user.as_ref().and_then(|user| user.uuid.as_deref()))?;
            let actor = to_actor(user.as_ref())?;
            Some(PullRequestReviewerCandidate {
                is_bot: actor.is_bot,
                login: actor.login,
                name: actor.name,
                avatar_url: actor.avatar_url,
                id: uuid,
                kind: PullRequestReviewerKind::User,
                is_requested: false,
            })
        })
        .collect();
    Ok(BitbucketPage {
        items,
        next: trimmed(page.next.as_deref()),
    })
}

/// `RawCommentSchema`: one comment as Bitbucket sent it, kept so threads can be assembled across
/// pages.
#[derive(Debug, Clone, PartialEq)]
pub struct BitbucketRawComment {
    pub id: i64,
    pub raw: Option<String>,
    pub user: Option<RawUser>,
    pub created_on: String,
    pub deleted: Option<bool>,
    /// A comment still being drafted by its author.
    pub pending: Option<bool>,
    /// Set on a reply, to the comment it answers: outer `None` when absent, `Some(None)` when
    /// `null`.
    pub parent: Option<Option<i64>>,
    pub inline: Option<RawInline>,
    /// Non-null once someone has marked the thread resolved.
    pub resolved: bool,
    pub html_href: Option<String>,
}

/// A comment's `inline` anchor.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawInline {
    pub path: Option<String>,
    /// The line in the file as it was; set instead of `to` on a removed line.
    pub from: Option<i64>,
    /// The line in the file as it is now.
    pub to: Option<i64>,
    pub outdated: Option<bool>,
}

fn decode_raw_comment(value: &Value) -> Decoded<BitbucketRawComment> {
    let map = object(value, "comment")?;
    let raw = match optional_object(map, "content")? {
        None => None,
        Some(content) => optional_string(content, "raw", false)?,
    };
    let parent = match map.get("parent") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(parent) => Some(Some(int(required(object(parent, "parent")?, "id")?, "parent.id")?)),
    };
    let inline = match optional_object(map, "inline")? {
        None => None,
        Some(inline) => Some(RawInline {
            path: optional_string(inline, "path", true)?,
            from: optional_int(inline, "from")?,
            to: optional_int(inline, "to")?,
            outdated: match optional(inline, "outdated", true)? {
                None => None,
                Some(value) => Some(value.as_bool().ok_or(Mismatch("outdated"))?),
            },
        }),
    };
    let html_href = match optional_object(map, "links")? {
        None => None,
        Some(links) => match optional_object(links, "html")? {
            None => None,
            Some(html) => optional_string(html, "href", false)?,
        },
    };
    Ok(BitbucketRawComment {
        id: int(required(map, "id")?, "id")?,
        raw,
        user: optional_user(map, "user")?,
        created_on: string(map, "created_on")?,
        deleted: optional_bool(map, "deleted")?,
        pending: optional_bool(map, "pending")?,
        parent,
        inline,
        resolved: !matches!(map.get("resolution"), None | Some(Value::Null)),
        html_href,
    })
}

/// A page of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct BitbucketComments {
    pub comments: Vec<PullRequestComment>,
    /// The same comments unread, for [`build_review_threads`]: a reply and the remark it answers
    /// can land on different pages, and only the caller holding every page can put them together.
    pub entries: Vec<BitbucketRawComment>,
    pub next: Option<String>,
}

/// `buildReviewThreads`: Bitbucket returns one flat list, so a thread is reassembled from it: a
/// comment pinned to a line opens a thread, and every reply that leads back to it belongs in it.
/// A reply whose parent is on a page that was not read has nowhere to go, and is left out rather
/// than shown as a thread of its own (it still stands in the flat conversation).
pub fn build_review_threads<'a>(comments: &'a [BitbucketRawComment]) -> Vec<PullRequestReviewThread> {
    let by_id: HashMap<i64, &'a BitbucketRawComment> = comments.iter().map(|comment| (comment.id, comment)).collect();
    // Bounded by the number of comments read, so a parent cycle cannot spin here.
    let root_of = |comment: &'a BitbucketRawComment| -> &'a BitbucketRawComment {
        let mut current = comment;
        for _ in 0..by_id.len() {
            let parent = match current.parent {
                Some(None) => None,
                // `byId.get(current.parent?.id ?? -1)`.
                Some(Some(id)) => by_id.get(&id),
                None => by_id.get(&-1),
            };
            match parent {
                None => return current,
                Some(parent) => current = parent,
            }
        }
        current
    };

    // `Map`s keep their first insertion's place.
    let mut thread_order: Vec<i64> = Vec::new();
    let mut threads: HashMap<i64, PullRequestReviewThread> = HashMap::new();
    let mut replies: HashMap<i64, Vec<&BitbucketRawComment>> = HashMap::new();
    for comment in comments {
        let root = root_of(comment);
        let inline = root.inline.as_ref();
        let Some(path) = trimmed(inline.and_then(|inline| inline.path.as_deref())) else {
            continue;
        };
        if root.id == comment.id {
            // `to` is the line as the file stands now, `from` the line it replaced; a comment that
            // carries only `from` was written against the removed side.
            let side = if inline.and_then(|inline| inline.to).is_none() {
                PullRequestDiffSide::Left
            } else {
                PullRequestDiffSide::Right
            };
            let line = match side {
                PullRequestDiffSide::Left => inline.and_then(|inline| inline.from),
                PullRequestDiffSide::Right => inline.and_then(|inline| inline.to),
            };
            if !threads.contains_key(&root.id) {
                thread_order.push(root.id);
            }
            threads.insert(
                root.id,
                PullRequestReviewThread {
                    id: root.id.to_string(),
                    path,
                    line: line.filter(|line| *line > 0),
                    side,
                    is_resolved: root.resolved,
                    is_outdated: inline.and_then(|inline| inline.outdated) == Some(true),
                    comments: Vec::new(),
                    comment_count: None,
                    next_comments_cursor: None,
                },
            );
        }
        replies.entry(root.id).or_default().push(comment);
    }

    thread_order
        .into_iter()
        .filter_map(|id| {
            let mut thread = threads.remove(&id)?;
            let mut entries = replies.remove(&id).unwrap_or_default();
            entries.sort_by(|left, right| left.created_on.cmp(&right.created_on));
            thread.comments = entries
                .into_iter()
                .map(|comment| PullRequestThreadComment {
                    id: comment.id.to_string(),
                    author: to_actor(comment.user.as_ref()),
                    body: comment.raw.clone().unwrap_or_default(),
                    created_at: to_iso_utc(&comment.created_on),
                    url: trimmed(comment.html_href.as_deref()),
                    reactions: None,
                })
                .collect();
            (!thread.comments.is_empty()).then_some(thread)
        })
        .collect()
}

/// `decodeCommentsJson`: deleted comments and ones their author has not posted yet carry nothing
/// to show. A comment pinned to a file is a line-level review comment.
pub fn decode_comments_json(raw: &str) -> Result<BitbucketComments, DecodeFailure> {
    let page = decode_page(raw)?;
    let mut comments = Vec::new();
    let mut kept = Vec::new();
    for entry in &page.values {
        let Ok(comment) = decode_raw_comment(entry) else { continue };
        if comment.deleted == Some(true) || comment.pending == Some(true) {
            continue;
        }
        let body = comment.raw.clone().unwrap_or_default();
        if js_trim(&body).is_empty() {
            continue;
        }
        let path = trimmed(comment.inline.as_ref().and_then(|inline| inline.path.as_deref()));
        comments.push(PullRequestComment {
            id: comment.id.to_string(),
            kind: if path.is_none() {
                PullRequestCommentKind::IssueComment
            } else {
                PullRequestCommentKind::ReviewComment
            },
            author: to_actor(comment.user.as_ref()),
            body,
            created_at: to_iso_utc(&comment.created_on),
            url: trimmed(comment.html_href.as_deref()),
            path,
            review_state: None,
            reactions: None,
        });
        kept.push(comment);
    }
    Ok(BitbucketComments {
        comments,
        entries: kept,
        next: trimmed(page.next.as_deref()),
    })
}

/// `RawCommitSchema`.
struct RawCommit {
    hash: String,
    message: Option<String>,
    date: Option<String>,
    author_raw: Option<String>,
    author_user: Option<RawUser>,
}

fn decode_raw_commit(entry: &Value) -> Decoded<RawCommit> {
    let map = object(entry, "commit")?;
    let (author_raw, author_user) = match optional_object(map, "author")? {
        None => (None, None),
        Some(author) => (optional_string(author, "raw", true)?, optional_user(author, "user")?),
    };
    Ok(RawCommit {
        hash: trimmed_string(map, "hash")?,
        message: optional_string(map, "message", true)?,
        date: optional_string(map, "date", true)?,
        author_raw,
        author_user,
    })
}

/// `decodeCommitsJson`: oldest first (Bitbucket lists a pull request's commits newest first).
pub fn decode_commits_json(raw: &str) -> Result<BitbucketPage<PullRequestCommit>, DecodeFailure> {
    let page = decode_page(raw)?;
    let mut commits = Vec::new();
    for entry in &page.values {
        let Ok(RawCommit {
            hash,
            message,
            date,
            author_raw,
            author_user,
        }) = decode_raw_commit(entry)
        else {
            continue;
        };
        let Some(committed_date) = trimmed(date.as_deref()) else { continue };
        let linked_author = to_actor(author_user.as_ref());
        let raw_author = trimmed(author_raw.as_deref());
        let authors = match (linked_author, raw_author) {
            (Some(actor), _) => vec![actor],
            (None, None) => Vec::new(),
            (None, Some(raw)) => vec![PullRequestActor {
                is_bot: None,
                login: raw.clone(),
                name: Some(raw),
                avatar_url: None,
            }],
        };
        commits.push(PullRequestCommit {
            oid: hash,
            message_headline: message.unwrap_or_default().split('\n').next().unwrap_or_default().to_owned(),
            committed_date: to_iso_utc(&committed_date),
            additions: None,
            deletions: None,
            authors: Some(authors),
        });
    }
    commits.reverse();
    Ok(BitbucketPage {
        items: commits,
        next: trimmed(page.next.as_deref()),
    })
}

/// `decodeStatusesJson`: build statuses as checks.
pub fn decode_statuses_json(raw: &str) -> Result<BitbucketPage<PullRequestCheck>, DecodeFailure> {
    let page = decode_page(raw)?;
    let mut checks = Vec::new();
    for entry in &page.values {
        let decoded = (|| -> Decoded<[Option<String>; 5]> {
            let map = object(entry, "status")?;
            Ok([
                optional_string(map, "key", true)?,
                optional_string(map, "name", true)?,
                optional_string(map, "state", true)?,
                optional_string(map, "description", true)?,
                optional_string(map, "url", true)?,
            ])
        })();
        let Ok([key, name, state, description, url]) = decoded else { continue };
        let Some(name) = trimmed(name.as_deref()).or_else(|| trimmed(key.as_deref())) else {
            continue;
        };
        // Bitbucket re-uses a status key when a pipeline is run again, so the same check can
        // appear twice on one page. Nothing decoded here says which copy is newer, so the later
        // one wins, which is the order Bitbucket writes an update in. The key is kept as the
        // workflow name so two pipelines that display the same name are not folded into one.
        checks.push(CheckEntry {
            check: PullRequestCheck {
                name,
                status: to_build_status(state.as_deref()),
                description: trimmed(description.as_deref()),
                url: trimmed(url.as_deref()),
            },
            workflow_name: trimmed(key.as_deref()),
            at: None,
        });
    }
    Ok(BitbucketPage {
        items: dedupe_checks(&checks),
        next: trimmed(page.next.as_deref()),
    })
}

/// A pull request's line counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BitbucketDiffStat {
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
}

/// One diffstat page: its totals and the next page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketDiffStatPage {
    pub stat: BitbucketDiffStat,
    pub next: Option<String>,
}

/// `decodeDiffstatJson`: one entry per changed file, each carrying that file's line counts.
pub fn decode_diffstat_json(raw: &str) -> Result<BitbucketDiffStatPage, DecodeFailure> {
    let page = decode_page(raw)?;
    let mut stat = BitbucketDiffStat::default();
    for entry in &page.values {
        let decoded = (|| -> Decoded<(Option<i64>, Option<i64>)> {
            let map = object(entry, "diffstat")?;
            Ok((optional_int(map, "lines_added")?, optional_int(map, "lines_removed")?))
        })();
        let Ok((added, removed)) = decoded else { continue };
        stat.additions += added.unwrap_or(0);
        stat.deletions += removed.unwrap_or(0);
        stat.changed_files += 1;
    }
    Ok(BitbucketDiffStatPage {
        stat,
        next: trimmed(page.next.as_deref()),
    })
}

/// `decodeConflictsJson`: one entry per conflicting path, so an empty page is the only statement
/// Bitbucket makes that a pull request merges cleanly.
pub fn decode_conflicts_json(raw: &str) -> Result<PullRequestMergeability, DecodeFailure> {
    let page = decode_page(raw)?;
    Ok(if page.values.is_empty() {
        PullRequestMergeability::Mergeable
    } else {
        PullRequestMergeability::Conflicting
    })
}

#[cfg(test)]
mod tests {
    //! `bitbucketPullRequestJson.test.ts`.

    use serde_json::json;

    use super::*;

    /// Shaped after a pull request as the Bitbucket API answers it, trimmed to the fields read.
    fn pull_request(overrides: Value) -> Value {
        let mut value = json!({
            "id": 897,
            "title": "Add widget-pipe",
            "description": "# Add widget-pipe",
            "state": "OPEN",
            "draft": false,
            "created_on": "2026-06-16T05:04:32.258456+00:00",
            "updated_on": "2026-06-16T05:04:33.750542+00:00",
            "author": {"display_name": "Avery Stone", "nickname": "avery", "type": "user"},
            "source": {"branch": {"name": "feat/page"}, "repository": {"full_name": "fork/web"}},
            "destination": {"branch": {"name": "master"}},
            "links": {"html": {"href": "https://bitbucket.example.test/acme/web/pull-requests/897"}},
        });
        if let (Value::Object(base), Value::Object(extra)) = (&mut value, overrides) {
            base.extend(extra);
        }
        value
    }

    fn page(values: Vec<Value>, extra: Value) -> String {
        let mut value = json!({"pagelen": 50, "page": 1, "size": values.len(), "values": values});
        if let (Value::Object(base), Value::Object(extra)) = (&mut value, extra) {
            base.extend(extra);
        }
        value.to_string()
    }

    fn actor(login: &str, name: Option<&str>) -> PullRequestActor {
        PullRequestActor {
            is_bot: None,
            login: login.into(),
            name: name.map(Into::into),
            avatar_url: None,
        }
    }

    #[test]
    fn reads_a_pull_request_as_a_change_request() {
        let decoded = decode_pull_request_page_json(&page(vec![pull_request(json!({}))], json!({}))).unwrap();
        assert_eq!(decoded.items.len(), 1);
        let item = &decoded.items[0];
        assert_eq!(item.number, 897);
        assert_eq!(item.title, "Add widget-pipe");
        assert_eq!(item.url, "https://bitbucket.example.test/acme/web/pull-requests/897");
        assert_eq!(
            item.author.as_ref().map(|a| (a.login.as_str(), a.name.as_deref())),
            Some(("avery", Some("Avery Stone")))
        );
        assert_eq!(item.head_branch, "feat/page");
        assert_eq!(item.head_repository_name_with_owner.as_deref(), Some("fork/web"));
        assert_eq!(item.base_branch, "master");
        assert_eq!(item.state, PullRequestState::Open);
        assert!(!item.is_draft);
        // Bitbucket says nothing about conflicts on the pull request itself.
        assert_eq!(item.mergeability, PullRequestMergeability::Unknown);
        assert_eq!(decoded.next, None);
    }

    #[test]
    fn normalizes_bitbucket_offset_timestamps() {
        let decoded = decode_pull_request_page_json(&page(vec![pull_request(json!({}))], json!({}))).unwrap();
        assert_eq!(decoded.items[0].created_at, "2026-06-16T05:04:32.258Z");
        assert_eq!(decoded.items[0].updated_at, "2026-06-16T05:04:33.750Z");
    }

    #[test]
    fn reports_the_next_page_as_the_whole_url_bitbucket_sends() {
        let next = "https://api.bitbucket.example.test/2.0/repositories/acme/web/pullrequests?page=2";
        let decoded = decode_pull_request_page_json(&page(vec![pull_request(json!({}))], json!({"next": next}))).unwrap();
        assert_eq!(decoded.next.as_deref(), Some(next));
    }

    #[test]
    fn reads_each_state() {
        for (state, expected) in [
            ("MERGED", PullRequestState::Merged),
            ("DECLINED", PullRequestState::Closed),
            ("SUPERSEDED", PullRequestState::Closed),
            ("OPEN", PullRequestState::Open),
            ("something new", PullRequestState::Open),
        ] {
            let decoded = decode_pull_request_page_json(&page(vec![pull_request(json!({"state": state}))], json!({}))).unwrap();
            assert_eq!(decoded.items[0].state, expected, "{state}");
        }
    }

    #[test]
    fn skips_a_malformed_row_rather_than_failing_the_page() {
        let decoded = decode_pull_request_page_json(&page(vec![json!({"id": "not a number"}), pull_request(json!({}))], json!({}))).unwrap();
        assert_eq!(decoded.items.len(), 1);
    }

    #[test]
    fn fails_when_bitbucket_did_not_answer_with_a_page() {
        assert!(decode_pull_request_page_json(&json!({"error": "nope"}).to_string()).is_err());
    }

    #[test]
    fn reads_reviewers_as_review_requests() {
        let decoded = decode_pull_request_json(&pull_request(json!({"reviewers": [{"nickname": "julius", "display_name": "Julius"}]})).to_string()).unwrap();
        assert_eq!(decoded.review_request_logins, vec!["julius".to_owned()]);
        assert_eq!(decoded.reviewers, vec![actor("julius", Some("Julius"))]);
    }

    #[test]
    fn reads_a_participant_vote_as_a_review() {
        let decoded = decode_pull_request_json(
            &pull_request(json!({"participants": [
                {"user": {"nickname": "julius", "display_name": "Julius"}, "role": "REVIEWER", "approved": true, "state": "approved", "participated_on": "2026-06-17T09:00:00+00:00"},
                // Added as a reviewer but has not voted, so there is no verdict to show.
                {"user": {"nickname": "sam", "display_name": "Sam"}, "role": "REVIEWER", "approved": false, "state": null, "participated_on": null},
            ]}))
            .to_string(),
        )
        .unwrap();
        assert_eq!(decoded.reviews.len(), 1);
        let review = &decoded.reviews[0];
        assert_eq!(review.kind, PullRequestCommentKind::Review);
        assert_eq!(review.author.as_ref().map(|a| a.login.as_str()), Some("julius"));
        assert_eq!(review.review_state.as_deref(), Some("approved"));
        assert_eq!(review.created_at, "2026-06-17T09:00:00.000Z");
    }

    #[test]
    fn reads_the_signed_in_nickname() {
        assert_eq!(
            decode_viewer_json(r#"{"nickname":"avery","display_name":"Avery"}"#).unwrap().as_deref(),
            Some("avery")
        );
    }

    #[test]
    fn falls_back_to_the_display_name_which_app_accounts_have_instead() {
        assert_eq!(decode_viewer_json(r#"{"display_name":"Release Bot"}"#).unwrap().as_deref(), Some("Release Bot"));
    }

    #[test]
    fn returns_nothing_when_the_account_has_neither() {
        assert_eq!(decode_viewer_json("{}").unwrap(), None);
    }

    #[test]
    fn keeps_a_posted_comment_and_drops_deleted_and_unposted_ones() {
        let decoded = decode_comments_json(&page(
            vec![
                json!({
                    "id": 797230941, "content": {"raw": "The issue is ready for review."},
                    "user": {"display_name": "Release Bot", "type": "app_user"},
                    "created_on": "2026-05-15T01:58:38.220690+00:00", "deleted": false, "pending": false,
                    "links": {"html": {"href": "https://bitbucket.example.test/acme/web/pull-requests/892#c1"}},
                }),
                json!({"id": 2, "content": {"raw": "gone"}, "created_on": "2026-05-15T02:00:00+00:00", "deleted": true}),
                json!({"id": 3, "content": {"raw": "wip"}, "created_on": "2026-05-15T02:00:00+00:00", "pending": true}),
                json!({"id": 4, "content": {"raw": "   "}, "created_on": "2026-05-15T02:00:00+00:00"}),
            ],
            json!({}),
        ))
        .unwrap();
        assert_eq!(decoded.comments.len(), 1);
        let comment = &decoded.comments[0];
        assert_eq!(comment.id, "797230941");
        assert_eq!(comment.kind, PullRequestCommentKind::IssueComment);
        // An app account has no nickname, so its display name is the only handle it has.
        assert_eq!(comment.author.as_ref().map(|a| a.login.as_str()), Some("Release Bot"));
        assert_eq!(comment.created_at, "2026-05-15T01:58:38.220Z");
    }

    #[test]
    fn reads_a_comment_pinned_to_a_file_as_a_review_comment() {
        let decoded = decode_comments_json(&page(
            vec![json!({"id": 5, "content": {"raw": "Rename this."}, "created_on": "2026-05-15T02:00:00+00:00", "inline": {"path": "src/app.ts"}})],
            json!({}),
        ))
        .unwrap();
        assert_eq!(decoded.comments[0].kind, PullRequestCommentKind::ReviewComment);
        assert_eq!(decoded.comments[0].path.as_deref(), Some("src/app.ts"));
    }

    #[test]
    fn returns_commits_oldest_first_with_only_the_subject_line() {
        let decoded = decode_commits_json(&page(
            vec![
                json!({"hash": "bbb", "message": "second\n\nbody text\n", "date": "2026-06-16T04:51:00+00:00"}),
                json!({
                    "hash": "aaa", "message": "first\n", "date": "2026-06-16T04:50:49+00:00",
                    "author": {"raw": "Ada Example <ada@example.test>", "user": {"nickname": "ada", "display_name": "Ada Example"}},
                }),
            ],
            json!({}),
        ))
        .unwrap();
        assert_eq!(decoded.items.iter().map(|c| c.oid.as_str()).collect::<Vec<_>>(), vec!["aaa", "bbb"]);
        assert_eq!(decoded.items[0].authors, Some(vec![actor("ada", Some("Ada Example"))]));
        assert_eq!(decoded.items[1].message_headline, "second");
        assert_eq!(decoded.next, None);
    }

    #[test]
    fn skips_commits_whose_hash_is_empty() {
        let decoded = decode_commits_json(&page(
            vec![
                json!({"hash": "   ", "message": "invalid", "date": "2026-06-16T04:51:00+00:00"}),
                json!({"hash": "aaa", "date": "2026-06-16T04:50:49+00:00"}),
            ],
            json!({}),
        ))
        .unwrap();
        assert_eq!(decoded.items.iter().map(|c| c.oid.as_str()).collect::<Vec<_>>(), vec!["aaa"]);
    }

    #[test]
    fn reads_a_build_status_as_a_check() {
        let decoded = decode_statuses_json(&page(
            vec![json!({
                "key": "custom:check-version-and-pr", "name": "Pipeline - custom: check-version-and-pr", "state": "SUCCESSFUL",
                "description": "", "url": "https://bitbucket.example.test/acme/web/pipelines/results/8126",
            })],
            json!({}),
        ))
        .unwrap();
        assert_eq!(
            decoded,
            BitbucketPage {
                items: vec![PullRequestCheck {
                    name: "Pipeline - custom: check-version-and-pr".into(),
                    status: PullRequestCheckStatus::Success,
                    description: None,
                    url: Some("https://bitbucket.example.test/acme/web/pipelines/results/8126".into()),
                }],
                next: None,
            }
        );
    }

    #[test]
    fn reads_each_build_state() {
        for (state, expected) in [
            ("SUCCESSFUL", PullRequestCheckStatus::Success),
            ("FAILED", PullRequestCheckStatus::Failure),
            ("INPROGRESS", PullRequestCheckStatus::Pending),
            ("STOPPED", PullRequestCheckStatus::Cancelled),
            ("something new", PullRequestCheckStatus::Neutral),
        ] {
            let decoded = decode_statuses_json(&page(vec![json!({"name": "Pipeline", "state": state})], json!({}))).unwrap();
            assert_eq!(decoded.items[0].status, expected, "{state}");
        }
    }

    #[test]
    fn keeps_two_statuses_that_share_a_display_name_but_have_different_keys() {
        let decoded = decode_statuses_json(&page(
            vec![
                json!({"key": "build", "name": "Pipeline", "state": "SUCCESSFUL"}),
                json!({"key": "deploy", "name": "Pipeline", "state": "FAILED"}),
            ],
            json!({}),
        ))
        .unwrap();
        assert_eq!(
            decoded.items.iter().map(|check| (check.name.as_str(), check.status)).collect::<Vec<_>>(),
            vec![
                ("build / Pipeline", PullRequestCheckStatus::Success),
                ("deploy / Pipeline", PullRequestCheckStatus::Failure)
            ]
        );
    }

    #[test]
    fn adds_up_the_per_file_counts() {
        let decoded = decode_diffstat_json(&page(
            vec![json!({"lines_added": 9, "lines_removed": 2}), json!({"lines_added": 32, "lines_removed": 14})],
            json!({}),
        ))
        .unwrap();
        assert_eq!(
            decoded,
            BitbucketDiffStatPage {
                stat: BitbucketDiffStat {
                    additions: 41,
                    deletions: 16,
                    changed_files: 2
                },
                next: None
            }
        );
    }

    #[test]
    fn calls_an_empty_conflict_list_mergeable_and_any_conflict_conflicting() {
        assert_eq!(decode_conflicts_json(&page(vec![], json!({}))).unwrap(), PullRequestMergeability::Mergeable);
        assert_eq!(
            decode_conflicts_json(&page(vec![json!({"path": "src/app.ts"})], json!({}))).unwrap(),
            PullRequestMergeability::Conflicting
        );
    }

    #[test]
    fn counts_admin_and_write_as_write_and_read_as_not() {
        let permission_page = |permission: &str| page(vec![json!({"type": "repository_permission", "permission": permission})], json!({}));
        assert!(decode_repository_permission_json(&permission_page("admin")).unwrap());
        assert!(decode_repository_permission_json(&permission_page("write")).unwrap());
        assert!(!decode_repository_permission_json(&permission_page("read")).unwrap());
    }

    #[test]
    fn grants_write_where_bitbucket_named_no_permission_at_all() {
        // An empty page is Bitbucket declining to say, which is an unknown standing rather than a
        // refusal — and an unknown one is granted.
        assert!(decode_repository_permission_json(&page(vec![], json!({}))).unwrap());
    }
}
