//! `pullRequest/azureDevOpsPullRequestJson.ts`: decoding what `az repos pr` and `az devops invoke`
//! answer with.
//!
//! Azure's enums are decoded as plain strings and normalized here, in the same tolerant style as
//! the other hosts: a new merge status must not fail a whole payload. Every field beyond the
//! identity is optional, because `az repos pr` returns more or less of the REST object depending
//! on the command. The schemas are checked the way Effect Schema checks them: an optional field
//! may be absent or `null`, but present with the wrong type fails the struct it is in.

use serde_json::{Map, Value};
use zc_contracts::{PullRequestActor, PullRequestComment, PullRequestCommentKind, PullRequestMergeMethod, PullRequestMergeability, PullRequestState};
use zc_sourcecontrol::azure::pull_requests::{azure_devops_pull_request_web_url, AzurePullRequestUrlInput};
use zc_sourcecontrol::util::{js_trim, safe_int};

use crate::azure::util::locale_compare;

/// What a failed decode reports (the message of the `SchemaError` cause).
pub type DecodeFailure = String;

/// The change kinds of an iteration's changes, which are exactly the contract's
/// `PullRequestDiffFileContentsInput["changeType"]` (the provider hands one to the other as is).
pub type AzureDevOpsChangeKind = zc_contracts::PullRequestDiffFileContentsInputChangeType;

/// Where a repository lives, in the terms Azure's REST routes address it by: the project and the
/// repository travel together as separate route parameters.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AzureDevOpsRepositoryLocation {
    pub project: String,
    pub repository: String,
}

/// One pull request, normalized.
#[derive(Debug, Clone, PartialEq)]
pub struct AzureDevOpsPullRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    pub created_at: String,
    /// Azure records no last-touched time, so the closing time stands in where there is one and
    /// the creation time otherwise.
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub body: String,
    pub review_request_logins: Vec<String>,
    pub reviewers: Vec<PullRequestActor>,
    /// Where this pull request lives, when Azure said enough to work it out.
    pub location: Option<AzureDevOpsRepositoryLocation>,
    /// Whether Azure is set to complete this on its own once its policies pass.
    pub auto_merge_enabled: bool,
    /// The completion strategy Azure stored with auto-complete, where it reported one.
    pub auto_merge_method: Option<PullRequestMergeMethod>,
}

/// `AzureDevOpsPullRequestBatch`.
#[derive(Debug, Clone, PartialEq)]
pub struct AzureDevOpsPullRequestBatch {
    pub items: Vec<AzureDevOpsPullRequest>,
    /// Zero-based positions of the decoded items in Azure's raw page.
    pub raw_indexes: Vec<usize>,
    /// Rows Azure returned, counted before decoding, so a skipped row cannot hide a next page.
    pub raw_count: usize,
}

/// The head and the merge base of one iteration (one push), the range its patch is taken over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureDevOpsIteration {
    pub id: i64,
    pub head_commit: String,
    pub merge_base_commit: String,
}

/// What one file did across an iteration. `old_path` differs from `path` only for a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureDevOpsChangeEntry {
    pub path: String,
    pub old_path: String,
    pub change_kind: AzureDevOpsChangeKind,
    pub object_id: Option<String>,
    pub original_object_id: Option<String>,
}

/// One page of what an iteration changed, and where the next one starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureDevOpsChangePage {
    pub changes: Vec<AzureDevOpsChangeEntry>,
    pub next_skip: Option<i64>,
}

/// One file's text at one commit, and whether Azure says the text is text at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AzureDevOpsItemContent {
    pub contents: String,
    pub is_binary: bool,
}

/// A value that does not match its schema.
struct Mismatch;

type Decoded<T> = Result<T, Mismatch>;

fn object(value: &Value) -> Decoded<&Map<String, Value>> {
    value.as_object().ok_or(Mismatch)
}

/// `Schema.optional(Schema.NullOr(X))`: absent and `null` are `None`.
fn nullable<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.get(key).filter(|value| !value.is_null())
}

fn opt_string<'a>(map: &'a Map<String, Value>, key: &str) -> Decoded<Option<&'a str>> {
    match nullable(map, key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(Mismatch),
    }
}

fn opt_bool(map: &Map<String, Value>, key: &str) -> Decoded<Option<bool>> {
    match nullable(map, key) {
        None => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(Mismatch),
    }
}

fn opt_int(map: &Map<String, Value>, key: &str) -> Decoded<Option<i64>> {
    match nullable(map, key) {
        None => Ok(None),
        Some(value) => safe_int(value).map(Some).ok_or(Mismatch),
    }
}

fn opt_object<'a>(map: &'a Map<String, Value>, key: &str) -> Decoded<Option<&'a Map<String, Value>>> {
    nullable(map, key).map(object).transpose()
}

fn required_int(map: &Map<String, Value>, key: &str) -> Decoded<i64> {
    map.get(key).and_then(safe_int).ok_or(Mismatch)
}

/// `TrimmedNonEmptyString`: trimmed, then refused when empty.
fn trimmed_non_empty(map: &Map<String, Value>, key: &str) -> Decoded<String> {
    let text = js_trim(map.get(key).and_then(Value::as_str).ok_or(Mismatch)?);
    if text.is_empty() {
        Err(Mismatch)
    } else {
        Ok(text.to_owned())
    }
}

/// `{"<key>": Array(Unknown)}`, the envelope of every list route.
fn array_field<'a>(map: &'a Map<String, Value>, key: &str) -> Decoded<&'a Vec<Value>> {
    map.get(key).and_then(Value::as_array).ok_or(Mismatch)
}

/// `trimmed`: the trimmed text, `None` for absent, `null` or blank.
fn trimmed(value: Option<&str>) -> Option<String> {
    let text = js_trim(value.unwrap_or_default());
    (!text.is_empty()).then(|| text.to_owned())
}

/// `JSON.parse` behind `decodeJsonResult`.
fn parse(raw: &str) -> Result<Value, DecodeFailure> {
    serde_json::from_str(raw).map_err(|error| error.to_string())
}

fn schema_failure(_: Mismatch) -> DecodeFailure {
    "Expected the response to match its schema".to_owned()
}

/// `RawIdentitySchema`.
#[derive(Default)]
struct RawIdentity<'a> {
    display_name: Option<&'a str>,
    /// An email or UPN, which is what `az account show` reports for the signed-in user.
    unique_name: Option<&'a str>,
    image_url: Option<&'a str>,
}

fn identity(value: &Value) -> Decoded<RawIdentity<'_>> {
    let map = object(value)?;
    Ok(RawIdentity {
        display_name: opt_string(map, "displayName")?,
        unique_name: opt_string(map, "uniqueName")?,
        image_url: opt_string(map, "imageUrl")?,
    })
}

fn opt_identity<'a>(map: &'a Map<String, Value>, key: &str) -> Decoded<Option<RawIdentity<'a>>> {
    nullable(map, key).map(identity).transpose()
}

/// `RawPullRequestSchema`.
struct RawPullRequest<'a> {
    pull_request_id: i64,
    title: &'a str,
    description: Option<&'a str>,
    status: Option<&'a str>,
    is_draft: Option<bool>,
    /// Who armed auto-complete: present while the pull request is set to complete on its own,
    /// left out once nobody has.
    auto_complete_set_by: Option<RawIdentity<'a>>,
    merge_strategy: Option<&'a str>,
    squash_merge: Option<bool>,
    merge_status: Option<&'a str>,
    created_by: Option<RawIdentity<'a>>,
    reviewers: Vec<RawIdentity<'a>>,
    // Required and non-empty: the wire contract will not carry a change request without a branch
    // or a created time, so a row missing one is skipped.
    source_ref_name: String,
    target_ref_name: String,
    creation_date: String,
    closed_date: Option<&'a str>,
    url: Option<&'a str>,
    repository_name: Option<&'a str>,
    repository_web_url: Option<&'a str>,
    project_name: Option<&'a str>,
    web_link: Option<&'a str>,
}

fn raw_pull_request(value: &Value) -> Decoded<RawPullRequest<'_>> {
    let map = object(value)?;
    let pull_request_id = required_int(map, "pullRequestId")?;
    let title = map.get("title").and_then(Value::as_str).ok_or(Mismatch)?;
    let description = opt_string(map, "description")?;
    let status = opt_string(map, "status")?;
    let is_draft = opt_bool(map, "isDraft")?;
    let auto_complete_set_by = opt_identity(map, "autoCompleteSetBy")?;
    let (merge_strategy, squash_merge) = match opt_object(map, "completionOptions")? {
        None => (None, None),
        Some(options) => (opt_string(options, "mergeStrategy")?, opt_bool(options, "squashMerge")?),
    };
    let merge_status = opt_string(map, "mergeStatus")?;
    let created_by = opt_identity(map, "createdBy")?;
    let reviewers = match nullable(map, "reviewers") {
        None => Vec::new(),
        Some(Value::Array(entries)) => entries.iter().map(identity).collect::<Decoded<Vec<_>>>()?,
        Some(_) => return Err(Mismatch),
    };
    let source_ref_name = trimmed_non_empty(map, "sourceRefName")?;
    let target_ref_name = trimmed_non_empty(map, "targetRefName")?;
    let creation_date = trimmed_non_empty(map, "creationDate")?;
    let closed_date = opt_string(map, "closedDate")?;
    let url = opt_string(map, "url")?;
    let (repository_name, repository_web_url, project_name) = match opt_object(map, "repository")? {
        None => (None, None, None),
        Some(repository) => {
            let project = match opt_object(repository, "project")? {
                None => None,
                Some(project) => opt_string(project, "name")?,
            };
            (opt_string(repository, "name")?, opt_string(repository, "webUrl")?, project)
        }
    };
    let web_link = match opt_object(map, "_links")? {
        None => None,
        Some(links) => match opt_object(links, "web")? {
            None => None,
            // `href: Schema.optional(Schema.String)`: absent is fine, `null` is not.
            Some(web) => match web.get("href") {
                None => None,
                Some(Value::String(href)) => Some(href.as_str()),
                Some(_) => return Err(Mismatch),
            },
        },
    };
    Ok(RawPullRequest {
        pull_request_id,
        title,
        description,
        status,
        is_draft,
        auto_complete_set_by,
        merge_strategy,
        squash_merge,
        merge_status,
        created_by,
        reviewers,
        source_ref_name,
        target_ref_name,
        creation_date,
        closed_date,
        url,
        repository_name,
        repository_web_url,
        project_name,
        web_link,
    })
}

fn normalize_ref_name(ref_name: &str) -> String {
    let text = js_trim(ref_name);
    text.strip_prefix("refs/heads/").unwrap_or(text).to_owned()
}

/// A login has to compare against `az account show`, which reports an email.
fn to_actor(raw: Option<&RawIdentity<'_>>) -> Option<PullRequestActor> {
    let raw = raw?;
    let login = trimmed(raw.unique_name).or_else(|| trimmed(raw.display_name))?;
    Some(PullRequestActor {
        is_bot: None,
        login,
        name: trimmed(raw.display_name),
        avatar_url: trimmed(raw.image_url),
    })
}

fn to_state(status: Option<&str>) -> PullRequestState {
    match status.map(|s| js_trim(s).to_lowercase()).as_deref() {
        Some("completed") => PullRequestState::Merged,
        Some("abandoned") => PullRequestState::Closed,
        _ => PullRequestState::Open,
    }
}

fn to_mergeability(value: Option<&str>) -> PullRequestMergeability {
    match value.map(|s| js_trim(s).to_lowercase()).as_deref() {
        Some("succeeded") => PullRequestMergeability::Mergeable,
        Some("conflicts" | "failure" | "rejectedbypolicy") => PullRequestMergeability::Conflicting,
        // `queued` and `notSet` mean Azure has not finished checking.
        _ => PullRequestMergeability::Unknown,
    }
}

/// Where a pull request's own repository sits, taken from what Azure returned rather than from
/// the local remote, whose shape differs between the modern, legacy and SSH forms.
fn to_location(raw: &RawPullRequest<'_>) -> Option<AzureDevOpsRepositoryLocation> {
    Some(AzureDevOpsRepositoryLocation {
        project: trimmed(raw.project_name)?,
        repository: trimmed(raw.repository_name)?,
    })
}

fn to_auto_merge_method(raw: &RawPullRequest<'_>) -> Option<PullRequestMergeMethod> {
    raw.auto_complete_set_by.as_ref()?;
    match raw.merge_strategy.map(|s| js_trim(s).to_lowercase()).as_deref() {
        Some("squash") => Some(PullRequestMergeMethod::Squash),
        Some("rebase" | "rebasemerge") => Some(PullRequestMergeMethod::Rebase),
        Some("nofastforward") => Some(PullRequestMergeMethod::Merge),
        _ => (raw.squash_merge == Some(true)).then_some(PullRequestMergeMethod::Squash),
    }
}

/// `None` when Azure said too little to place the pull request: a row with no browser url and no
/// branch left after its prefix is dropped cannot be rendered or opened.
fn to_pull_request(raw: &RawPullRequest<'_>) -> Option<AzureDevOpsPullRequest> {
    let reviewers: Vec<PullRequestActor> = raw.reviewers.iter().filter_map(|reviewer| to_actor(Some(reviewer))).collect();
    let closed_at = trimmed(raw.closed_date);
    let url = trimmed(Some(&azure_devops_pull_request_web_url(&AzurePullRequestUrlInput {
        pull_request_id: raw.pull_request_id,
        web_link: raw.web_link,
        repository_web_url: raw.repository_web_url,
        rest_api_url: raw.url,
        project_name: raw.project_name,
        repository_name: raw.repository_name,
    })))?;
    let head_branch = trimmed(Some(&normalize_ref_name(&raw.source_ref_name)))?;
    let base_branch = trimmed(Some(&normalize_ref_name(&raw.target_ref_name)))?;
    Some(AzureDevOpsPullRequest {
        number: raw.pull_request_id,
        title: raw.title.to_owned(),
        url,
        author: to_actor(raw.created_by.as_ref()),
        head_branch,
        base_branch,
        state: to_state(raw.status),
        is_draft: raw.is_draft.unwrap_or(false),
        mergeability: to_mergeability(raw.merge_status),
        created_at: raw.creation_date.clone(),
        updated_at: closed_at.clone().unwrap_or_else(|| raw.creation_date.clone()),
        closed_at,
        body: raw.description.unwrap_or_default().to_owned(),
        review_request_logins: reviewers.iter().map(|reviewer| reviewer.login.clone()).collect(),
        reviewers,
        location: to_location(raw),
        auto_merge_enabled: raw.auto_complete_set_by.is_some(),
        auto_merge_method: to_auto_merge_method(raw),
    })
}

/// `decodePullRequestListJson`: malformed entries are skipped rather than failing the batch.
pub fn decode_pull_request_list_json(raw: &str) -> Result<AzureDevOpsPullRequestBatch, DecodeFailure> {
    let value = parse(raw)?;
    let entries = value.as_array().ok_or_else(|| schema_failure(Mismatch))?;
    let mut items = Vec::new();
    let mut raw_indexes = Vec::new();
    for (raw_index, entry) in entries.iter().enumerate() {
        let Ok(decoded) = raw_pull_request(entry) else { continue };
        if let Some(pull_request) = to_pull_request(&decoded) {
            items.push(pull_request);
            raw_indexes.push(raw_index);
        }
    }
    Ok(AzureDevOpsPullRequestBatch {
        items,
        raw_indexes,
        raw_count: entries.len(),
    })
}

/// `decodePullRequestJson`: `Ok(None)` carries "Azure answered, but with too little to use".
pub fn decode_pull_request_json(raw: &str) -> Result<Option<AzureDevOpsPullRequest>, DecodeFailure> {
    let value = parse(raw)?;
    let decoded = raw_pull_request(&value).map_err(schema_failure)?;
    Ok(to_pull_request(&decoded))
}

/// `decodeViewerJson`: `{"user": {"name": …}}`, the signed-in account, whose name is an email.
pub fn decode_viewer_json(raw: &str) -> Result<Option<String>, DecodeFailure> {
    let value = parse(raw)?;
    let decode = || -> Decoded<Option<String>> {
        let map = object(&value)?;
        Ok(match opt_object(map, "user")? {
            None => None,
            Some(user) => trimmed(opt_string(user, "name")?),
        })
    };
    decode().map_err(schema_failure)
}

/// One comment of `RawThreadSchema`.
struct RawComment<'a> {
    id: Option<i64>,
    content: Option<&'a str>,
    author: Option<RawIdentity<'a>>,
    published_date: Option<&'a str>,
    is_deleted: Option<bool>,
    /// `system` marks the notes Azure writes itself, which are events, not comments.
    comment_type: Option<&'a str>,
}

struct RawThread<'a> {
    id: i64,
    is_deleted: Option<bool>,
    file_path: Option<&'a str>,
    comments: Vec<RawComment<'a>>,
}

fn raw_thread(value: &Value) -> Decoded<RawThread<'_>> {
    let map = object(value)?;
    let id = required_int(map, "id")?;
    let is_deleted = opt_bool(map, "isDeleted")?;
    let file_path = match opt_object(map, "threadContext")? {
        None => None,
        Some(context) => opt_string(context, "filePath")?,
    };
    let comments = match nullable(map, "comments") {
        None => Vec::new(),
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|entry| {
                let comment = object(entry)?;
                Ok(RawComment {
                    id: opt_int(comment, "id")?,
                    content: opt_string(comment, "content")?,
                    author: opt_identity(comment, "author")?,
                    published_date: opt_string(comment, "publishedDate")?,
                    is_deleted: opt_bool(comment, "isDeleted")?,
                    comment_type: opt_string(comment, "commentType")?,
                })
            })
            .collect::<Decoded<Vec<_>>>()?,
        Some(_) => return Err(Mismatch),
    };
    Ok(RawThread {
        id,
        is_deleted,
        file_path,
        comments,
    })
}

/// `decodeThreadsJson`: every remark of every thread (a reply is as much of the conversation as
/// the line that opened it), oldest first. A thread pinned to a file is a review comment. Azure
/// answers the whole collection at once, so this is everything the host has.
pub fn decode_threads_json(raw: &str) -> Result<Vec<PullRequestComment>, DecodeFailure> {
    let value = parse(raw)?;
    let entries = object(&value).and_then(|map| array_field(map, "value")).map_err(schema_failure)?;
    let mut comments = Vec::new();
    for entry in entries {
        let Ok(thread) = raw_thread(entry) else { continue };
        if thread.is_deleted == Some(true) {
            continue;
        }
        let path = trimmed(thread.file_path);
        for comment in &thread.comments {
            let published_date = trimmed(comment.published_date);
            let system = comment.comment_type.is_some_and(|kind| js_trim(kind).to_lowercase() == "system");
            let blank = js_trim(comment.content.unwrap_or_default()).is_empty();
            let Some(published_date) = published_date.filter(|_| comment.is_deleted != Some(true) && !system && !blank) else {
                continue;
            };
            comments.push(PullRequestComment {
                id: format!("{}:{}", thread.id, comment.id.unwrap_or(0)),
                kind: if path.is_none() {
                    PullRequestCommentKind::IssueComment
                } else {
                    PullRequestCommentKind::ReviewComment
                },
                author: to_actor(comment.author.as_ref()),
                body: comment.content.unwrap_or_default().to_owned(),
                created_at: published_date,
                url: None,
                path: path.clone(),
                review_state: None,
                reactions: None,
            });
        }
    }
    comments.sort_by(|left, right| locale_compare(&left.created_at, &right.created_at));
    Ok(comments)
}

fn commit_id(map: &Map<String, Value>, key: &str) -> Decoded<Option<String>> {
    Ok(match opt_object(map, key)? {
        None => None,
        Some(commit) => trimmed(opt_string(commit, "commitId")?),
    })
}

/// `decodeIterationsJson`: every push, oldest first. An iteration Azure cannot place both ends
/// of names no range, and a patch needs both, so it is skipped.
pub fn decode_iterations_json(raw: &str) -> Result<Vec<AzureDevOpsIteration>, DecodeFailure> {
    let value = parse(raw)?;
    let entries = object(&value).and_then(|map| array_field(map, "value")).map_err(schema_failure)?;
    let mut iterations = Vec::new();
    for entry in entries {
        let decoded = (|| -> Decoded<(i64, Option<String>, Option<String>)> {
            let map = object(entry)?;
            Ok((required_int(map, "id")?, commit_id(map, "sourceRefCommit")?, commit_id(map, "commonRefCommit")?))
        })();
        let Ok((id, Some(head_commit), Some(merge_base_commit))) = decoded else {
            continue;
        };
        iterations.push(AzureDevOpsIteration {
            id,
            head_commit,
            merge_base_commit,
        });
    }
    iterations.sort_by_key(|iteration| iteration.id);
    Ok(iterations)
}

/// Azure leads a path with a slash that every other host and every patch omits. Not trimmed,
/// since a leading or trailing space is a legal part of a file's name.
fn to_repository_path(value: Option<&str>) -> Option<String> {
    let path = value?.trim_start_matches('/');
    (!path.is_empty()).then(|| path.to_owned())
}

/// Azure names a change with one word or two, and a rename arrives alone or with the edit that
/// came with it. Anything newer reads as a plain change, which shows the file.
fn to_change_kind(raw: Option<&str>, renamed: bool) -> AzureDevOpsChangeKind {
    let lowered = raw.unwrap_or_default().to_lowercase();
    let parts: Vec<&str> = lowered.split(',').map(js_trim).filter(|part| !part.is_empty()).collect();
    let has = |word: &str| parts.contains(&word);
    if has("delete") {
        AzureDevOpsChangeKind::Deleted
    } else if renamed || has("rename") {
        if has("edit") {
            AzureDevOpsChangeKind::RenameChanged
        } else {
            AzureDevOpsChangeKind::RenamePure
        }
    } else if has("add") {
        AzureDevOpsChangeKind::New
    } else {
        AzureDevOpsChangeKind::Change
    }
}

/// One entry of `RawChangeEntrySchema`, as the fields this reads.
struct RawChange<'a> {
    change_type: Option<&'a str>,
    source_server_item: Option<&'a str>,
    /// Where a renamed file came from, as an iteration's changes state it.
    original_path: Option<&'a str>,
    path: Option<&'a str>,
    object_id: Option<&'a str>,
    original_object_id: Option<&'a str>,
    is_folder: Option<bool>,
    git_object_type: Option<&'a str>,
}

fn raw_change(value: &Value) -> Decoded<RawChange<'_>> {
    let map = object(value)?;
    let change_type = opt_string(map, "changeType")?;
    let source_server_item = opt_string(map, "sourceServerItem")?;
    let original_path = opt_string(map, "originalPath")?;
    let (path, object_id, original_object_id, is_folder, git_object_type) = match opt_object(map, "item")? {
        None => (None, None, None, None, None),
        Some(item) => (
            opt_string(item, "path")?,
            opt_string(item, "objectId")?,
            opt_string(item, "originalObjectId")?,
            opt_bool(item, "isFolder")?,
            opt_string(item, "gitObjectType")?,
        ),
    };
    Ok(RawChange {
        change_type,
        source_server_item,
        original_path,
        path,
        object_id,
        original_object_id,
        is_folder,
        git_object_type,
    })
}

/// `decodeIterationChangesJson`: the files one page of an iteration changed (folders and
/// non-blob entries dropped), and where the next page starts.
pub fn decode_iteration_changes_json(raw: &str) -> Result<AzureDevOpsChangePage, DecodeFailure> {
    let value = parse(raw)?;
    let (entries, next_skip) = (|| -> Decoded<(&Vec<Value>, Option<f64>)> {
        let map = object(&value)?;
        let next_skip = match nullable(map, "nextSkip") {
            None => None,
            Some(number) => Some(number.as_f64().ok_or(Mismatch)?),
        };
        Ok((array_field(map, "changeEntries")?, next_skip))
    })()
    .map_err(schema_failure)?;
    let mut changes = Vec::new();
    for entry in entries {
        let Ok(change) = raw_change(entry) else { continue };
        let Some(path) = to_repository_path(change.path) else { continue };
        // A review shows files, and a folder has no content on either side to show for one.
        if change.is_folder == Some(true) {
            continue;
        }
        if change.git_object_type.unwrap_or("blob").to_lowercase() != "blob" {
            continue;
        }
        // Azure names where a renamed file came from in either of two places depending on the
        // route and the version; the current path stands in when neither is there.
        let old_path = to_repository_path(change.source_server_item)
            .or_else(|| to_repository_path(change.original_path))
            .unwrap_or_else(|| path.clone());
        let renamed = old_path != path;
        changes.push(AzureDevOpsChangeEntry {
            change_kind: to_change_kind(change.change_type, renamed),
            object_id: trimmed(change.object_id),
            original_object_id: trimmed(change.original_object_id),
            path,
            old_path,
        });
    }
    let next_skip = next_skip
        .filter(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && *n > 0.0)
        .map(|n| n as i64);
    Ok(AzureDevOpsChangePage { changes, next_skip })
}

/// `decodeItemContentJson`: an absent file arrives as an empty body, which reads as empty; whether
/// the bytes are text is Azure's own call (`contentMetadata.isBinary`).
pub fn decode_item_content_json(raw: &str) -> Result<AzureDevOpsItemContent, DecodeFailure> {
    let value = parse(raw)?;
    let decode = || -> Decoded<AzureDevOpsItemContent> {
        let map = object(&value)?;
        let contents = opt_string(map, "content")?.unwrap_or_default().to_owned();
        let is_binary = match opt_object(map, "contentMetadata")? {
            None => false,
            Some(metadata) => opt_bool(metadata, "isBinary")? == Some(true),
        };
        Ok(AzureDevOpsItemContent { contents, is_binary })
    };
    decode().map_err(schema_failure)
}
