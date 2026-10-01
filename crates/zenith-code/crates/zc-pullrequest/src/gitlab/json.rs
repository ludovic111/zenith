//! `pullRequest/gitLabMergeRequestJson.ts`: decoding what `glab api` answers into the neutral
//! pull request shapes.
//!
//! GitLab's REST enums are decoded as plain strings and normalized here: a GitLab release that
//! adds a pipeline status or a merge status must not fail the whole payload. Each raw schema is
//! decoded with the TS `Schema.Struct` rules (see [`super::util`]); list endpoints skip a
//! malformed row rather than failing the batch, and report the raw row count alongside so a
//! skipped row cannot hide a next page. Decode failures are the error strings of the
//! `Schema` cause the TS keeps.

use serde_json::{Map, Value};
use zc_contracts::{
    PullRequestActor, PullRequestCheck, PullRequestCheckStatus, PullRequestComment, PullRequestCommentKind, PullRequestCommit, PullRequestDiffSide,
    PullRequestLabel, PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestMergeability, PullRequestReaction, PullRequestReactionContent,
    PullRequestReviewThread, PullRequestReviewerCandidate, PullRequestReviewerKind, PullRequestState, PullRequestThreadComment,
};
use zc_sourcecontrol::util::js_trim;

use super::util::{
    array, ensure_trailing_newline, int, js_parse_int, nullable_item, nullable_struct, object, opt_array, opt_bool, opt_int, opt_string, opt_struct,
    parse_list, parse_struct, quote_git_patch_path, required_struct, string, string_item, trimmed, Decoded, Mismatch, OrderedMap,
};

/// A decode failure (the TS `Cause<SchemaError>`), as its message.
pub type DecodeFailure = String;

/// `RawUserSchema`. GitLab writes reviewers as numeric ids, so the id rides along with the handle.
#[derive(Debug, Clone)]
struct RawUser {
    id: Option<i64>,
    username: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

fn raw_user(object: &Map<String, Value>) -> Decoded<RawUser> {
    Ok(RawUser {
        id: opt_int(object, "id", false)?,
        username: string(object, "username")?,
        name: opt_string(object, "name", true)?,
        avatar_url: opt_string(object, "avatar_url", true)?,
    })
}

fn raw_user_item(value: &Value) -> Decoded<RawUser> {
    raw_user(object(value)?)
}

/// `RawPipelineSchema`.
#[derive(Debug, Clone)]
struct RawPipeline {
    status: Option<String>,
    web_url: Option<String>,
    source: Option<String>,
}

/// `RawMergeRequestSchema`.
#[derive(Debug, Clone)]
struct RawMergeRequest {
    iid: i64,
    title: String,
    web_url: String,
    description: Option<String>,
    author: Option<RawUser>,
    source_branch: String,
    target_branch: String,
    state: Option<String>,
    draft: Option<bool>,
    work_in_progress: Option<bool>,
    merge_status: Option<String>,
    has_conflicts: Option<bool>,
    created_at: String,
    updated_at: String,
    merged_at: Option<String>,
    closed_at: Option<String>,
    reviewers: Option<Vec<RawUser>>,
    labels: Option<Vec<String>>,
    /// A string, and `"1000+"` past GitLab's counting limit, so it is parsed rather than decoded.
    changes_count: Option<String>,
    head_pipeline: Option<RawPipeline>,
    /// `user`: present (`Some`) with its `can_merge`, which only the single-merge-request
    /// endpoint carries and which already accounts for role, approval rules and protected branches.
    user: Option<Option<bool>>,
    /// `merge_when_pipeline_succeeds`, which every version answers; newer ones also send
    /// `auto_merge_enabled` for the same fact.
    merge_when_pipeline_succeeds: Option<bool>,
    auto_merge_enabled: Option<bool>,
    /// The stored squash choice, including project-policy overrides.
    squash_on_merge: Option<bool>,
    /// How far the target branch has moved on; only answered for a single merge request asked
    /// with `include_diverged_commits_count`.
    diverged_commits_count: Option<i64>,
}

fn raw_merge_request(object: &Map<String, Value>) -> Decoded<RawMergeRequest> {
    // `squash` is declared (so a non-boolean fails the row) but not read.
    opt_bool(object, "squash", true)?;
    Ok(RawMergeRequest {
        iid: int(object, "iid")?,
        title: string(object, "title")?,
        web_url: string(object, "web_url")?,
        description: opt_string(object, "description", true)?,
        author: opt_struct(object, "author", true, raw_user)?,
        source_branch: string(object, "source_branch")?,
        target_branch: string(object, "target_branch")?,
        state: opt_string(object, "state", true)?,
        draft: opt_bool(object, "draft", false)?,
        work_in_progress: opt_bool(object, "work_in_progress", false)?,
        merge_status: opt_string(object, "merge_status", true)?,
        has_conflicts: opt_bool(object, "has_conflicts", true)?,
        created_at: string(object, "created_at")?,
        updated_at: string(object, "updated_at")?,
        merged_at: opt_string(object, "merged_at", true)?,
        closed_at: opt_string(object, "closed_at", true)?,
        reviewers: opt_array(object, "reviewers", true, raw_user_item)?,
        labels: opt_array(object, "labels", true, string_item)?,
        changes_count: opt_string(object, "changes_count", true)?,
        head_pipeline: opt_struct(object, "head_pipeline", true, |pipeline| {
            Ok(RawPipeline {
                status: opt_string(pipeline, "status", true)?,
                web_url: opt_string(pipeline, "web_url", true)?,
                source: opt_string(pipeline, "source", true)?,
            })
        })?,
        user: opt_struct(object, "user", true, |user| opt_bool(user, "can_merge", false))?,
        merge_when_pipeline_succeeds: opt_bool(object, "merge_when_pipeline_succeeds", true)?,
        auto_merge_enabled: opt_bool(object, "auto_merge_enabled", true)?,
        squash_on_merge: opt_bool(object, "squash_on_merge", true)?,
        diverged_commits_count: opt_int(object, "diverged_commits_count", true)?,
    })
}

/// A merge request as a listing row.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabMergeRequestListItem {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    /// GitLab reports no line counts on a merge request, so both stay zero and the surface omits
    /// the stat. The Code tab counts them from the patch it already fetched.
    pub additions: i64,
    pub deletions: i64,
    pub created_at: String,
    pub updated_at: String,
    pub review_request_logins: Vec<String>,
    pub labels: Vec<PullRequestLabel>,
}

/// A merge request as its detail.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabMergeRequestDetail {
    pub item: GitLabMergeRequestListItem,
    pub body: String,
    pub changed_files: i64,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub reviewers: Vec<PullRequestActor>,
    pub checks: Vec<PullRequestCheck>,
    /// False only where GitLab said so; an answer without the field leaves merging permitted.
    pub viewer_can_merge: bool,
    /// The reviewers as GitLab addresses them, which is what writing the set back takes.
    pub reviewer_ids: Vec<i64>,
    /// Absent where GitLab named neither auto-merge field, which is not the same as off.
    pub auto_merge_enabled: Option<bool>,
    /// GitLab only exposes the stored strategy separately when that strategy is squash.
    pub auto_merge_method: Option<PullRequestMergeMethod>,
    /// Absent where GitLab did not count, which is not a branch with nothing behind it.
    pub diverged_commits: Option<i64>,
}

fn to_actor(raw: Option<&RawUser>) -> Option<PullRequestActor> {
    let raw = raw?;
    let login = trimmed(Some(&raw.username))?;
    Some(PullRequestActor {
        is_bot: None,
        login,
        name: trimmed(raw.name.as_deref()),
        avatar_url: trimmed(raw.avatar_url.as_deref()),
    })
}

fn lower_trimmed(value: Option<&str>) -> Option<String> {
    value.map(|text| js_trim(text).to_lowercase())
}

fn to_state(raw: &RawMergeRequest) -> PullRequestState {
    if trimmed(raw.merged_at.as_deref()).is_some() {
        return PullRequestState::Merged;
    }
    match lower_trimmed(raw.state.as_deref()).as_deref() {
        Some("merged") => PullRequestState::Merged,
        Some("closed") => PullRequestState::Closed,
        // `locked` is an open merge request whose discussion is locked.
        _ => PullRequestState::Open,
    }
}

fn to_mergeability(raw: &RawMergeRequest) -> PullRequestMergeability {
    if raw.has_conflicts == Some(true) {
        return PullRequestMergeability::Conflicting;
    }
    match lower_trimmed(raw.merge_status.as_deref()).as_deref() {
        Some("can_be_merged") => PullRequestMergeability::Mergeable,
        Some("cannot_be_merged") => PullRequestMergeability::Conflicting,
        // `unchecked` and `checking` mean GitLab has not finished the merge check yet.
        _ => PullRequestMergeability::Unknown,
    }
}

/// GitLab returns label names only, so there is no colour to carry.
fn to_labels(raw: Option<&[String]>) -> Vec<PullRequestLabel> {
    raw.unwrap_or_default()
        .iter()
        .filter_map(|label| trimmed(Some(label)).map(|name| PullRequestLabel { name, color: None }))
        .collect()
}

/// `"3"` for a counted change set, `"1000+"` once GitLab gives up counting: the leading number is
/// the floor either way.
fn to_changed_files(value: Option<&str>) -> i64 {
    let parsed = js_parse_int(js_trim(value.unwrap_or_default()));
    if parsed.is_finite() && parsed > 0.0 {
        parsed as i64
    } else {
        0
    }
}

fn to_pipeline_status(value: Option<&str>) -> PullRequestCheckStatus {
    match lower_trimmed(value).as_deref() {
        Some("success") => PullRequestCheckStatus::Success,
        Some("failed") => PullRequestCheckStatus::Failure,
        Some("canceled" | "cancelling") => PullRequestCheckStatus::Cancelled,
        Some("skipped") => PullRequestCheckStatus::Skipped,
        // A pipeline waiting on a person is not progress, and it is not a failure either.
        Some("manual" | "scheduled") => PullRequestCheckStatus::Neutral,
        _ => PullRequestCheckStatus::Pending,
    }
}

/// GitLab has no per-job check list on a merge request, so its pipeline is the one check.
fn to_checks(raw: &RawMergeRequest) -> Vec<PullRequestCheck> {
    let Some(pipeline) = &raw.head_pipeline else {
        return Vec::new();
    };
    vec![PullRequestCheck {
        name: "Pipeline".into(),
        status: to_pipeline_status(pipeline.status.as_deref()),
        description: trimmed(pipeline.source.as_deref()),
        url: trimmed(pipeline.web_url.as_deref()),
    }]
}

fn to_list_item(raw: &RawMergeRequest) -> GitLabMergeRequestListItem {
    GitLabMergeRequestListItem {
        number: raw.iid,
        title: raw.title.clone(),
        url: raw.web_url.clone(),
        author: to_actor(raw.author.as_ref()),
        head_branch: raw.source_branch.clone(),
        base_branch: raw.target_branch.clone(),
        state: to_state(raw),
        is_draft: raw.draft.or(raw.work_in_progress).unwrap_or(false),
        mergeability: to_mergeability(raw),
        additions: 0,
        deletions: 0,
        created_at: raw.created_at.clone(),
        updated_at: raw.updated_at.clone(),
        review_request_logins: raw
            .reviewers
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|reviewer| trimmed(Some(&reviewer.username)))
            .collect(),
        labels: to_labels(raw.labels.as_deref()),
    }
}

fn to_detail(raw: &RawMergeRequest) -> GitLabMergeRequestDetail {
    let auto_merge = if raw.merge_when_pipeline_succeeds.is_none() && raw.auto_merge_enabled.is_none() {
        None
    } else {
        Some(raw.merge_when_pipeline_succeeds == Some(true) || raw.auto_merge_enabled == Some(true))
    };
    let reviewers = raw.reviewers.as_deref().unwrap_or_default();
    GitLabMergeRequestDetail {
        item: to_list_item(raw),
        body: raw.description.clone().unwrap_or_default(),
        changed_files: to_changed_files(raw.changes_count.as_deref()),
        merged_at: trimmed(raw.merged_at.as_deref()),
        closed_at: trimmed(raw.closed_at.as_deref()),
        // Built from the reviewers themselves rather than their logins, so the avatars survive.
        reviewers: reviewers.iter().filter_map(|reviewer| to_actor(Some(reviewer))).collect(),
        checks: to_checks(raw),
        viewer_can_merge: raw.user != Some(Some(false)),
        reviewer_ids: reviewers.iter().filter_map(|reviewer| reviewer.id).collect(),
        auto_merge_enabled: auto_merge,
        auto_merge_method: (auto_merge == Some(true) && raw.squash_on_merge == Some(true)).then_some(PullRequestMergeMethod::Squash),
        diverged_commits: raw.diverged_commits_count,
    }
}

/// `GitLabMergeRequestListBatch` of the decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabMergeRequestListPage {
    pub items: Vec<GitLabMergeRequestListItem>,
    /// Zero-based positions of the decoded items in GitLab's raw page.
    pub raw_indexes: Vec<usize>,
    /// Rows GitLab returned, counted before decoding.
    pub raw_count: usize,
}

/// `decodeMergeRequestListJson`: malformed entries are skipped rather than failing the batch.
pub fn decode_merge_request_list_json(raw: &str) -> Result<GitLabMergeRequestListPage, DecodeFailure> {
    let rows = parse_list(raw)?;
    let mut items = Vec::new();
    let mut raw_indexes = Vec::new();
    for (raw_index, entry) in rows.iter().enumerate() {
        if let Ok(item) = object(entry).and_then(raw_merge_request) {
            items.push(to_list_item(&item));
            raw_indexes.push(raw_index);
        }
    }
    Ok(GitLabMergeRequestListPage {
        items,
        raw_indexes,
        raw_count: rows.len(),
    })
}

/// `decodeMergeRequestDetailJson`.
pub fn decode_merge_request_detail_json(raw: &str) -> Result<GitLabMergeRequestDetail, DecodeFailure> {
    parse_struct(raw, raw_merge_request).map(|raw| to_detail(&raw))
}

/// `decodeViewerJson`: the signed-in username, `None` when the account has none.
pub fn decode_viewer_json(raw: &str) -> Result<Option<String>, DecodeFailure> {
    parse_struct(raw, |viewer| opt_string(viewer, "username", true)).map(|username| trimmed(username.as_deref()))
}

/// `GitLabProjectUsers`.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabProjectUsers {
    pub candidates: Vec<PullRequestReviewerCandidate>,
    /// Rows GitLab returned, counted before decoding, so a skipped row cannot hide a next page.
    pub raw_count: usize,
}

/// `decodeProjectUsersJson`: the people with access to the project (`GET /projects/:id/users`),
/// nobody marked requested yet. A malformed row is skipped.
pub fn decode_project_users_json(raw: &str) -> Result<GitLabProjectUsers, DecodeFailure> {
    let rows = parse_list(raw)?;
    let mut candidates = Vec::new();
    for entry in &rows {
        let Ok(user) = raw_user_item(entry) else { continue };
        let Some(id) = user.id else { continue };
        let Some(actor) = to_actor(Some(&user)) else { continue };
        candidates.push(PullRequestReviewerCandidate {
            is_bot: None,
            login: actor.login,
            name: actor.name,
            avatar_url: actor.avatar_url,
            id: id.to_string(),
            kind: PullRequestReviewerKind::User,
            is_requested: false,
        });
    }
    Ok(GitLabProjectUsers {
        candidates,
        raw_count: rows.len(),
    })
}

/// `decodeProjectMergeCapabilitiesJson`: GitLab settles one strategy per project (`merge_method`:
/// merge commit, semi-linear or fast-forward) plus a separate squash switch. An unrecognized
/// setting offers nothing rather than a strategy the project forbids.
pub fn decode_project_merge_capabilities_json(raw: &str) -> Result<PullRequestMergeCapabilities, DecodeFailure> {
    let (merge_method, squash_option) = parse_struct(raw, |project| {
        Ok((opt_string(project, "merge_method", true)?, opt_string(project, "squash_option", true)?))
    })?;
    let merge_method = lower_trimmed(merge_method.as_deref());
    let squash_option = lower_trimmed(squash_option.as_deref());
    Ok(PullRequestMergeCapabilities {
        merge: merge_method.as_deref() == Some("merge"),
        // Both semi-linear and fast-forward histories are reached by rebasing onto the target.
        rebase: matches!(merge_method.as_deref(), Some("rebase_merge" | "ff")),
        squash: matches!(squash_option.as_deref(), Some("always" | "default_on" | "default_off")),
    })
}

/// The three revisions a positioned comment is written against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabDiffRefs {
    pub base_sha: String,
    pub head_sha: String,
    pub start_sha: String,
}

/// `GitLabDiscussions`.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabDiscussions {
    pub threads: Vec<PullRequestReviewThread>,
    /// Discussions GitLab returned, counted before decoding.
    pub raw_count: usize,
}

#[derive(Debug, Clone)]
struct RawPosition {
    position_type: Option<String>,
    new_path: Option<String>,
    old_path: Option<String>,
    new_line: Option<i64>,
    old_line: Option<i64>,
}

#[derive(Debug, Clone)]
struct RawDiscussionNote {
    id: i64,
    body: Option<String>,
    author: Option<RawUser>,
    created_at: String,
    system: Option<bool>,
    resolved: Option<bool>,
    position: Option<RawPosition>,
}

fn raw_discussion_note(value: &Value) -> Decoded<RawDiscussionNote> {
    let note = object(value)?;
    opt_bool(note, "resolvable", false)?;
    Ok(RawDiscussionNote {
        id: int(note, "id")?,
        body: opt_string(note, "body", true)?,
        author: opt_struct(note, "author", true, raw_user)?,
        created_at: string(note, "created_at")?,
        system: opt_bool(note, "system", false)?,
        resolved: opt_bool(note, "resolved", true)?,
        position: opt_struct(note, "position", true, |position| {
            Ok(RawPosition {
                position_type: opt_string(position, "position_type", true)?,
                new_path: opt_string(position, "new_path", true)?,
                old_path: opt_string(position, "old_path", true)?,
                new_line: opt_int(position, "new_line", true)?,
                old_line: opt_int(position, "old_line", true)?,
            })
        })?,
    })
}

/// `decodeDiscussionsJson`: positioned discussions only. GitLab returns the whole conversation
/// here, and only a positioned one belongs against a line of the diff.
pub fn decode_discussions_json(raw: &str) -> Result<GitLabDiscussions, DecodeFailure> {
    let rows = parse_list(raw)?;
    let mut threads = Vec::new();
    for entry in &rows {
        let Ok((id, notes)) = object(entry).and_then(|discussion| Ok((string(discussion, "id")?, opt_array(discussion, "notes", true, raw_discussion_note)?)))
        else {
            continue;
        };
        let notes: Vec<RawDiscussionNote> = notes.unwrap_or_default().into_iter().filter(|note| note.system != Some(true)).collect();
        let Some(root) = notes.first() else { continue };
        let Some(position) = &root.position else { continue };
        if position.position_type.as_deref() != Some("text") {
            continue;
        }
        // A comment on an added or context line carries `new_line`; one on a removed line only
        // `old_line`, and belongs against the file as it was.
        let side = if position.new_line.is_none() {
            PullRequestDiffSide::Left
        } else {
            PullRequestDiffSide::Right
        };
        let (path, line) = match side {
            PullRequestDiffSide::Left => (trimmed(position.old_path.as_deref()), position.old_line),
            PullRequestDiffSide::Right => (trimmed(position.new_path.as_deref()), position.new_line),
        };
        let Some(path) = path else { continue };
        threads.push(PullRequestReviewThread {
            id,
            path,
            line: line.filter(|line| *line > 0),
            side,
            is_resolved: root.resolved == Some(true),
            // GitLab reports no "outdated"; the diff works that out.
            is_outdated: false,
            comments: notes
                .iter()
                .map(|note| PullRequestThreadComment {
                    id: note.id.to_string(),
                    author: to_actor(note.author.as_ref()),
                    body: note.body.clone().unwrap_or_default(),
                    created_at: note.created_at.clone(),
                    url: None,
                    reactions: None,
                })
                .collect(),
            comment_count: None,
            next_comments_cursor: None,
        });
    }
    Ok(GitLabDiscussions {
        threads,
        raw_count: rows.len(),
    })
}

/// `decodeDiffRefsJson`: `None` for a merge request without diff refs.
pub fn decode_diff_refs_json(raw: &str) -> Result<Option<GitLabDiffRefs>, DecodeFailure> {
    parse_struct(raw, |merge_request| {
        opt_struct(merge_request, "diff_refs", true, |refs| {
            Ok(GitLabDiffRefs {
                base_sha: string(refs, "base_sha")?,
                head_sha: string(refs, "head_sha")?,
                start_sha: string(refs, "start_sha")?,
            })
        })
    })
}

/// `decodeNotesJson`'s answer.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabNotes {
    pub comments: Vec<PullRequestComment>,
    /// Notes GitLab returned, so dropped activity entries still say whether the page was full.
    pub raw_count: usize,
}

#[derive(Debug, Clone)]
struct RawNote {
    id: i64,
    body: Option<String>,
    author: Option<RawUser>,
    created_at: String,
    system: Option<bool>,
    kind: Option<String>,
    position: Option<(Option<String>, Option<String>)>,
}

fn raw_note(value: &Value) -> Decoded<RawNote> {
    let note = object(value)?;
    Ok(RawNote {
        id: int(note, "id")?,
        body: opt_string(note, "body", true)?,
        author: opt_struct(note, "author", true, raw_user)?,
        created_at: string(note, "created_at")?,
        system: opt_bool(note, "system", false)?,
        kind: opt_string(note, "type", true)?,
        position: opt_struct(note, "position", true, |position| {
            Ok((opt_string(position, "new_path", true)?, opt_string(position, "old_path", true)?))
        })?,
    })
}

/// `decodeNotesJson`: comments only. System notes are GitLab's own activity entries, and a
/// `DiffNote` is the root of a line-level discussion (a review comment).
pub fn decode_notes_json(raw: &str) -> Result<GitLabNotes, DecodeFailure> {
    let rows = parse_list(raw)?;
    let mut comments = Vec::new();
    for entry in &rows {
        let Ok(note) = raw_note(entry) else { continue };
        if note.system == Some(true) {
            continue;
        }
        let body = note.body.clone().unwrap_or_default();
        if js_trim(&body).is_empty() {
            continue;
        }
        let is_diff_note = note.kind.as_deref().map(js_trim) == Some("DiffNote");
        let (new_path, old_path) = note.position.clone().unwrap_or_default();
        comments.push(PullRequestComment {
            id: note.id.to_string(),
            kind: if is_diff_note {
                PullRequestCommentKind::ReviewComment
            } else {
                PullRequestCommentKind::IssueComment
            },
            author: to_actor(note.author.as_ref()),
            body,
            created_at: note.created_at,
            url: None,
            path: trimmed(new_path.as_deref()).or_else(|| trimmed(old_path.as_deref())),
            review_state: None,
            reactions: None,
        });
    }
    Ok(GitLabNotes {
        comments,
        raw_count: rows.len(),
    })
}

#[derive(Debug, Clone)]
struct RawCommit {
    id: String,
    title: Option<String>,
    committed_date: Option<String>,
    created_at: Option<String>,
    parent_ids: Option<Vec<String>>,
    author_name: Option<String>,
    author_email: Option<String>,
    stats: Option<(Option<i64>, Option<i64>)>,
}

fn raw_commit(commit: &Map<String, Value>) -> Decoded<RawCommit> {
    // `TrimmedNonEmptyString`: trimmed on decode, then non-empty.
    let id = trimmed(Some(&string(commit, "id")?)).ok_or(Mismatch)?;
    Ok(RawCommit {
        id,
        title: opt_string(commit, "title", true)?,
        committed_date: opt_string(commit, "committed_date", true)?,
        created_at: opt_string(commit, "created_at", true)?,
        parent_ids: opt_array(commit, "parent_ids", false, string_item)?,
        author_name: opt_string(commit, "author_name", true)?,
        author_email: opt_string(commit, "author_email", true)?,
        stats: opt_struct(commit, "stats", true, |stats| {
            Ok((opt_int(stats, "additions", false)?, opt_int(stats, "deletions", false)?))
        })?,
    })
}

/// `decodeCommitsJson`: oldest first (GitLab lists them newest first).
pub fn decode_commits_json(raw: &str) -> Result<Vec<PullRequestCommit>, DecodeFailure> {
    let rows = parse_list(raw)?;
    let mut commits = Vec::new();
    for entry in &rows {
        let Ok(commit) = object(entry).and_then(raw_commit) else { continue };
        let Some(committed_date) = trimmed(commit.committed_date.as_deref()).or_else(|| trimmed(commit.created_at.as_deref())) else {
            continue;
        };
        let login = trimmed(commit.author_name.as_deref()).or_else(|| trimmed(commit.author_email.as_deref()));
        commits.push(PullRequestCommit {
            oid: commit.id,
            message_headline: commit.title.unwrap_or_default(),
            committed_date,
            additions: commit.stats.map(|(additions, _)| additions.unwrap_or(0).max(0)),
            deletions: commit.stats.map(|(_, deletions)| deletions.unwrap_or(0).max(0)),
            authors: Some(match login {
                None => Vec::new(),
                Some(login) => vec![PullRequestActor {
                    is_bot: None,
                    login,
                    name: trimmed(commit.author_name.as_deref()),
                    avatar_url: None,
                }],
            }),
        });
    }
    commits.reverse();
    Ok(commits)
}

/// `decodeCommitDiffRefsJson`: the exact comparison GitLab uses for a commit-scoped diff, `None`
/// for a commit without a parent.
pub fn decode_commit_diff_refs_json(raw: &str) -> Result<Option<GitLabDiffRefs>, DecodeFailure> {
    let commit = parse_struct(raw, raw_commit)?;
    let base_sha = trimmed(commit.parent_ids.as_ref().and_then(|ids| ids.first()).map(String::as_str));
    let head_sha = trimmed(Some(&commit.id));
    Ok(match (base_sha, head_sha) {
        (Some(base_sha), Some(head_sha)) => Some(GitLabDiffRefs {
            start_sha: base_sha.clone(),
            base_sha,
            head_sha,
        }),
        _ => None,
    })
}

#[derive(Debug, Clone)]
struct RawDiff {
    old_path: String,
    new_path: String,
    a_mode: Option<String>,
    b_mode: Option<String>,
    new_file: Option<bool>,
    renamed_file: Option<bool>,
    deleted_file: Option<bool>,
    diff: Option<String>,
    /// GitLab omits the hunks for a file too large to inline.
    too_large: Option<bool>,
    /// And for one it collapsed.
    collapsed: Option<bool>,
}

fn raw_diff(value: &Value) -> Decoded<RawDiff> {
    let file = object(value)?;
    Ok(RawDiff {
        old_path: string(file, "old_path")?,
        new_path: string(file, "new_path")?,
        a_mode: opt_string(file, "a_mode", true)?,
        b_mode: opt_string(file, "b_mode", true)?,
        new_file: opt_bool(file, "new_file", false)?,
        renamed_file: opt_bool(file, "renamed_file", false)?,
        deleted_file: opt_bool(file, "deleted_file", false)?,
        diff: opt_string(file, "diff", true)?,
        too_large: opt_bool(file, "too_large", true)?,
        collapsed: opt_bool(file, "collapsed", true)?,
    })
}

/// `GitLabMergeRequestPatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabMergeRequestPatch {
    pub patch: String,
    /// At least one file's hunks were withheld by GitLab as too large to inline.
    pub truncated: bool,
    /// Files GitLab returned, counted before decoding, so the caller can page.
    pub raw_count: usize,
}

/// `decodeMergeRequestDiffsJson`: GitLab returns hunks per file with no `diff --git` header, so
/// the unified patch is assembled here, one page at a time.
pub fn decode_merge_request_diffs_json(raw: &str) -> Result<GitLabMergeRequestPatch, DecodeFailure> {
    let rows = parse_list(raw)?;
    let mut sections = Vec::new();
    let mut truncated = false;
    for entry in &rows {
        let Ok(file) = raw_diff(entry) else { continue };
        let hunks = file.diff.clone().unwrap_or_default();
        if hunks.is_empty() {
            // A file GitLab declined to inline still belongs in the file list, header only.
            truncated = truncated || file.too_large == Some(true) || file.collapsed == Some(true);
        }
        let from = if file.new_file == Some(true) {
            "/dev/null".to_owned()
        } else {
            quote_git_patch_path(&format!("a/{}", file.old_path))
        };
        let to = if file.deleted_file == Some(true) {
            "/dev/null".to_owned()
        } else {
            quote_git_patch_path(&format!("b/{}", file.new_path))
        };
        let mut header = vec![format!(
            "diff --git {} {}",
            quote_git_patch_path(&format!("a/{}", file.old_path)),
            quote_git_patch_path(&format!("b/{}", file.new_path))
        )];
        if file.new_file == Some(true) {
            header.push(format!("new file mode {}", file.b_mode.as_deref().unwrap_or("100644")));
        }
        if file.deleted_file == Some(true) {
            header.push(format!("deleted file mode {}", file.a_mode.as_deref().unwrap_or("100644")));
        }
        if file.renamed_file == Some(true) {
            header.push(format!("rename from {}", quote_git_patch_path(&file.old_path)));
            header.push(format!("rename to {}", quote_git_patch_path(&file.new_path)));
        }
        header.push(format!("--- {from}"));
        header.push(format!("+++ {to}"));
        let header = header.join("\n");
        sections.push(if hunks.is_empty() {
            header
        } else {
            format!("{header}\n{}", ensure_trailing_newline(&hunks))
        });
    }
    Ok(GitLabMergeRequestPatch {
        patch: sections.join("\n"),
        truncated,
        raw_count: rows.len(),
    })
}

/// GitLab's award names for the eight reactions the contract carries.
pub fn gitlab_award_name(content: PullRequestReactionContent) -> &'static str {
    match content {
        PullRequestReactionContent::ThumbsUp => "thumbsup",
        PullRequestReactionContent::ThumbsDown => "thumbsdown",
        PullRequestReactionContent::Laugh => "laughing",
        PullRequestReactionContent::Hooray => "tada",
        PullRequestReactionContent::Confused => "confused",
        PullRequestReactionContent::Heart => "heart",
        PullRequestReactionContent::Rocket => "rocket",
        PullRequestReactionContent::Eyes => "eyes",
    }
}

fn content_by_award(name: &str) -> Option<PullRequestReactionContent> {
    PullRequestReactionContent::ALL
        .iter()
        .copied()
        .find(|content| gitlab_award_name(*content) == name)
}

/// Awards on the merge request and on every note of it, in one read. `currentUser` rides along
/// because GitLab names who awarded but never says whether that is the reader.
pub const AWARD_EMOJI_GRAPHQL_QUERY: &str = "query($fullPath: ID!, $iid: String!, $cursor: String) {
  currentUser { username }
  project(fullPath: $fullPath) {
    mergeRequest(iid: $iid) {
      awardEmoji { nodes { name user { username } } }
      notes(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id awardEmoji { nodes { name user { username } } } }
      }
    }
  }
}";

/// `{name, user.username}` of one award node.
type RawAward = (Option<String>, Option<String>);

fn optional_username(object: &Map<String, Value>, key: &str) -> Decoded<Option<String>> {
    Ok(opt_struct(object, key, true, |user| opt_string(user, "username", true))?.flatten())
}

/// `RawAwardEmojiNodesSchema`.
fn raw_award_nodes(object: &Map<String, Value>, key: &str) -> Decoded<Vec<Option<RawAward>>> {
    Ok(opt_struct(object, key, true, |award_emoji| {
        opt_array(award_emoji, "nodes", true, |node| {
            nullable_item(node, |node| Ok((opt_string(node, "name", true)?, optional_username(node, "user")?)))
        })
    })?
    .flatten()
    .unwrap_or_default())
}

/// The awards on one subject grouped the way a reaction pill is drawn. The viewer's own username
/// is left out of `actors` (the page says "You") but still counted.
fn to_reactions(nodes: &[Option<RawAward>], viewer: Option<&str>) -> Vec<PullRequestReaction> {
    let normalized_viewer = viewer.map(str::to_lowercase);
    let mut groups: Vec<(PullRequestReactionContent, i64, Vec<String>, bool)> = Vec::new();
    for (name, username) in nodes.iter().flatten() {
        // An award outside the eight is left out: GitLab accepts any emoji, the others none.
        let Some(content) = content_by_award(&trimmed(name.as_deref()).map(|name| name.to_lowercase()).unwrap_or_default()) else {
            continue;
        };
        let Some(username) = trimmed(username.as_deref()) else { continue };
        let at = match groups.iter().position(|group| group.0 == content) {
            Some(at) => at,
            None => {
                groups.push((content, 0, Vec::new(), false));
                groups.len() - 1
            }
        };
        let group = &mut groups[at];
        group.1 += 1;
        if normalized_viewer.as_deref() == Some(username.to_lowercase().as_str()) {
            group.3 = true;
        } else {
            group.2.push(username);
        }
    }
    groups
        .into_iter()
        .filter(|group| group.1 > 0)
        .map(|(content, count, actors, viewer_has_reacted)| PullRequestReaction {
            content,
            count,
            actors,
            viewer_has_reacted,
        })
        .collect()
}

/// `gid://gitlab/DiffNote/42` is note 42, the id the REST conversation carries.
fn note_id_of(gid: Option<&str>) -> Option<String> {
    let gid = trimmed(gid)?;
    let id = gid.rsplit('/').next()?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.to_owned())
}

/// `GitLabAwardEmojiPage`.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabAwardEmojiPage {
    /// The merge request's own awards, which are the ones on its description.
    pub reactions: Vec<PullRequestReaction>,
    pub reactions_by_note_id: OrderedMap<Vec<PullRequestReaction>>,
    pub next_cursor: Option<String>,
}

struct RawAwardPage {
    viewer: Option<String>,
    merge_request: Option<RawAwardMergeRequest>,
}

/// `pageInfo`: `{hasNextPage, endCursor}`.
type RawPageInfo = (Option<bool>, Option<String>);
/// A note node: `{id, awardEmoji}`.
type RawAwardNote = (Option<String>, Vec<Option<RawAward>>);

struct RawAwardMergeRequest {
    award_emoji: Vec<Option<RawAward>>,
    notes: Option<(Option<RawPageInfo>, Vec<Option<RawAwardNote>>)>,
}

fn raw_award_page(page: &Map<String, Value>) -> Decoded<RawAwardPage> {
    required_struct(page, "data", |data| {
        let viewer = optional_username(data, "currentUser")?;
        let merge_request = nullable_struct(data, "project", |project| {
            nullable_struct(project, "mergeRequest", |merge_request| {
                Ok(RawAwardMergeRequest {
                    award_emoji: raw_award_nodes(merge_request, "awardEmoji")?,
                    notes: opt_struct(merge_request, "notes", true, |notes| {
                        let page_info = opt_struct(notes, "pageInfo", false, |info| {
                            Ok((opt_bool(info, "hasNextPage", false)?, opt_string(info, "endCursor", true)?))
                        })?;
                        let nodes = array(notes.get("nodes").ok_or(Mismatch)?, |node| {
                            nullable_item(node, |node| Ok((opt_string(node, "id", true)?, raw_award_nodes(node, "awardEmoji")?)))
                        })?;
                        Ok((page_info, nodes))
                    })?,
                })
            })
        })?
        .flatten();
        Ok(RawAwardPage { viewer, merge_request })
    })
}

/// `decodeAwardEmojiJson`.
pub fn decode_award_emoji_json(raw: &str) -> Result<GitLabAwardEmojiPage, DecodeFailure> {
    let page = parse_struct(raw, raw_award_page)?;
    let viewer = trimmed(page.viewer.as_deref());
    let mut reactions_by_note_id = OrderedMap::new();
    let mut next_cursor = None;
    let mut reactions = Vec::new();
    if let Some(merge_request) = &page.merge_request {
        if let Some((page_info, nodes)) = &merge_request.notes {
            for (id, award_emoji) in nodes.iter().flatten() {
                let Some(id) = note_id_of(id.as_deref()) else { continue };
                let note_reactions = to_reactions(award_emoji, viewer.as_deref());
                if !note_reactions.is_empty() {
                    reactions_by_note_id.set(id, note_reactions);
                }
            }
            if let Some((Some(true), end_cursor)) = page_info {
                next_cursor = trimmed(end_cursor.as_deref());
            }
        }
        reactions = to_reactions(&merge_request.award_emoji, viewer.as_deref());
    }
    Ok(GitLabAwardEmojiPage {
        reactions,
        reactions_by_note_id,
        next_cursor,
    })
}

/// `decodeOwnAwardIdJson`: the reader's own award of one name on a subject, which is what taking
/// a reaction back is addressed by (GitLab deletes an award by its id).
pub fn decode_own_award_id_json(raw: &str, content: PullRequestReactionContent, viewer: &str) -> Result<Option<i64>, DecodeFailure> {
    let rows = parse_list(raw)?;
    let name = gitlab_award_name(content);
    for entry in &rows {
        let Ok((id, award_name, username)) =
            object(entry).and_then(|award| Ok((int(award, "id")?, opt_string(award, "name", true)?, optional_username(award, "user")?)))
        else {
            continue;
        };
        if trimmed(award_name.as_deref()).map(|name| name.to_lowercase()).as_deref() != Some(name) {
            continue;
        }
        if trimmed(username.as_deref()).as_deref() != Some(viewer) {
            continue;
        }
        return Ok(Some(id));
    }
    Ok(None)
}

/// What the given paths are at one revision, as blob ids, asked for by path (GitLab charges the
/// query by how many paths it is given). A path the revision does not have comes back missing.
pub const REPOSITORY_BLOBS_GRAPHQL_QUERY: &str = "query($fullPath: ID!, $ref: String!, $paths: [String!]!) {
  project(fullPath: $fullPath) {
    repository {
      blobs(ref: $ref, paths: $paths) {
        nodes { path oid }
      }
    }
  }
}";

/// `decodeRepositoryBlobsJson`: blob ids by path, or `None` where GitLab did not answer the query
/// at all (a project the token cannot see), which must not read as "none of these files". Paths
/// are not trimmed: a leading or trailing space is a legal part of a file name.
pub fn decode_repository_blobs_json(raw: &str) -> Result<Option<OrderedMap<String>>, DecodeFailure> {
    let nodes = parse_struct(raw, |page| {
        required_struct(page, "data", |data| {
            Ok(nullable_struct(data, "project", |project| {
                Ok(opt_struct(project, "repository", true, |repository| {
                    Ok(opt_struct(repository, "blobs", true, |blobs| {
                        opt_array(blobs, "nodes", true, |node| {
                            nullable_item(node, |node| Ok((opt_string(node, "path", true)?, opt_string(node, "oid", true)?)))
                        })
                    })?
                    .flatten())
                })?
                .flatten())
            })?
            .flatten())
        })
    })?;
    let Some(nodes) = nodes else { return Ok(None) };
    let mut blobs = OrderedMap::new();
    for (path, oid) in nodes.into_iter().flatten() {
        let Some(path) = path.filter(|path| !path.is_empty()) else { continue };
        let Some(oid) = trimmed(oid.as_deref()) else { continue };
        blobs.set(path, oid);
    }
    Ok(Some(blobs))
}

#[cfg(test)]
#[path = "json_tests.rs"]
mod tests;
