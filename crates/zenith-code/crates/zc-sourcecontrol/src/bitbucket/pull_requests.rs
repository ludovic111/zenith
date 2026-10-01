//! `bitbucketPullRequests.ts`: Bitbucket Cloud pull request JSON.

use serde_json::{Map, Value};
use zc_contracts::{ChangeRequestState, DateTimeUtc};

use crate::records::NormalizedChangeRequest;
use crate::util::{
    js_trim, optional_bool, optional_date, optional_field, optional_string, positive_int, trim_optional_string, trimmed_non_empty, SchemaMismatch,
};

/// `BitbucketRepositoryRefSchema`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BitbucketRepositoryRef {
    pub full_name: Option<String>,
    pub workspace_slug: Option<String>,
}

/// `BitbucketPullRequestBranchSchema`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketBranch {
    pub repository: Option<BitbucketRepositoryRef>,
    pub branch_name: String,
}

/// `BitbucketPullRequestSchema`.
#[derive(Debug, Clone, PartialEq)]
pub struct BitbucketPullRequest {
    pub id: i64,
    pub title: String,
    pub state: Option<String>,
    pub draft: Option<bool>,
    pub updated_on: Option<DateTimeUtc>,
    pub html_url: String,
    pub source: BitbucketBranch,
    pub destination: BitbucketBranch,
}

fn optional_trimmed(map: &Map<String, Value>, key: &str) -> Result<Option<String>, SchemaMismatch> {
    match optional_field(map, key, true)? {
        None => Ok(None),
        Some(value) => trimmed_non_empty(value).map(Some).ok_or(SchemaMismatch),
    }
}

fn decode_repository_ref(map: &Map<String, Value>) -> Result<Option<BitbucketRepositoryRef>, SchemaMismatch> {
    let repository = match optional_field(map, "repository", true)? {
        None => return Ok(None),
        Some(Value::Object(repository)) => repository,
        Some(_) => return Err(SchemaMismatch),
    };
    let workspace_slug = match optional_field(repository, "workspace", true)? {
        None => None,
        Some(Value::Object(workspace)) => optional_trimmed(workspace, "slug")?,
        Some(_) => return Err(SchemaMismatch),
    };
    Ok(Some(BitbucketRepositoryRef {
        full_name: optional_trimmed(repository, "full_name")?,
        workspace_slug,
    }))
}

fn decode_branch(value: &Value) -> Option<BitbucketBranch> {
    let map = value.as_object()?;
    Some(BitbucketBranch {
        repository: decode_repository_ref(map).ok()?,
        branch_name: trimmed_non_empty(map.get("branch")?.as_object()?.get("name")?)?,
    })
}

/// Decodes one pull request (`None` when it does not match the schema).
pub fn decode_bitbucket_pull_request(value: &Value) -> Option<BitbucketPullRequest> {
    let map = value.as_object()?;
    Some(BitbucketPullRequest {
        id: positive_int(map.get("id")?)?,
        title: trimmed_non_empty(map.get("title")?)?,
        state: optional_string(map, "state", true).ok()?,
        draft: optional_bool(map, "draft").ok()?,
        updated_on: optional_date(map, "updated_on").ok()?,
        html_url: trimmed_non_empty(map.get("links")?.as_object()?.get("html")?.as_object()?.get("href")?)?,
        source: decode_branch(map.get("source")?)?,
        destination: decode_branch(map.get("destination")?)?,
    })
}

/// `BitbucketPullRequestListSchema`: every value must decode.
pub fn decode_bitbucket_pull_request_list(value: &Value) -> Option<Vec<BitbucketPullRequest>> {
    let map = value.as_object()?;
    if optional_trimmed(map, "next").is_err() || map.get("next").is_some_and(Value::is_null) {
        return None;
    }
    map.get("values")?.as_array()?.iter().map(decode_bitbucket_pull_request).collect()
}

fn repository_owner(repository: &BitbucketRepositoryRef) -> Option<String> {
    trim_optional_string(repository.workspace_slug.as_deref()).or_else(|| {
        repository
            .full_name
            .as_ref()
            .filter(|name| name.contains('/'))
            .and_then(|name| name.split('/').next().map(str::to_owned))
    })
}

fn normalize_state(state: Option<&str>) -> ChangeRequestState {
    match state.map(|s| js_trim(s).to_uppercase()).as_deref() {
        Some("MERGED") => ChangeRequestState::Merged,
        Some("DECLINED" | "SUPERSEDED") => ChangeRequestState::Closed,
        _ => ChangeRequestState::Open,
    }
}

/// `normalizeBitbucketPullRequestRecord`.
pub fn normalize_bitbucket_pull_request(raw: &BitbucketPullRequest) -> NormalizedChangeRequest {
    let head_name = trim_optional_string(raw.source.repository.as_ref().and_then(|r| r.full_name.as_deref()));
    let base_name = trim_optional_string(raw.destination.repository.as_ref().and_then(|r| r.full_name.as_deref()));
    let owner = raw.source.repository.as_ref().and_then(repository_owner);
    let is_cross_repository = matches!((&head_name, &base_name), (Some(head), Some(base)) if head != base);
    NormalizedChangeRequest {
        number: raw.id,
        title: raw.title.clone(),
        url: raw.html_url.clone(),
        base_ref_name: raw.destination.branch_name.clone(),
        head_ref_name: raw.source.branch_name.clone(),
        state: normalize_state(raw.state.as_deref()),
        is_draft: (raw.draft == Some(true)).then_some(true),
        closed_at: None,
        merged_at: None,
        updated_at: raw.updated_on,
        is_cross_repository: is_cross_repository.then_some(true),
        head_repository_name_with_owner: head_name.map(Some),
        head_repository_owner_login: owner.map(Some),
    }
}
