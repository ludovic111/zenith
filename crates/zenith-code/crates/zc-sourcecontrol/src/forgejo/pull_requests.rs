//! `forgejoPullRequests.ts`: the Forgejo/Gitea pull request JSON and its `ChangeRequest`.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};
use zc_contracts::{ChangeRequest, ChangeRequestState, DateTimeUtc, EOption, SourceControlProviderKind};

use crate::util::{optional_bool, optional_date, optional_string, safe_int};

/// `Repository` of a pull request branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoRepositoryRef {
    pub full_name: String,
    pub owner_login: String,
}

/// `Branch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgejoBranch {
    pub ref_name: String,
    pub sha: String,
    pub repo: Option<ForgejoRepositoryRef>,
}

/// `ForgejoPullRequestSchema`.
#[derive(Debug, Clone, PartialEq)]
pub struct ForgejoPullRequest {
    pub number: i64,
    pub title: String,
    pub html_url: String,
    pub state: String,
    pub merged: bool,
    pub draft: Option<bool>,
    pub base: ForgejoBranch,
    pub head: ForgejoBranch,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub updated_at: Option<DateTimeUtc>,
}

fn decode_branch(value: &Value) -> Option<ForgejoBranch> {
    let map = value.as_object()?;
    let repo = match map.get("repo")? {
        Value::Null => None,
        Value::Object(repo) => Some(ForgejoRepositoryRef {
            full_name: repo.get("full_name")?.as_str()?.to_owned(),
            owner_login: repo.get("owner")?.as_object()?.get("login")?.as_str()?.to_owned(),
        }),
        _ => return None,
    };
    Some(ForgejoBranch {
        ref_name: map.get("ref")?.as_str()?.to_owned(),
        sha: map.get("sha")?.as_str()?.to_owned(),
        repo,
    })
}

fn string(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)?.as_str().map(str::to_owned)
}

/// Decodes one pull request (`None` when it does not match the schema).
pub fn decode_forgejo_pull_request(value: &Value) -> Option<ForgejoPullRequest> {
    let map = value.as_object()?;
    Some(ForgejoPullRequest {
        number: safe_int(map.get("number")?)?,
        title: string(map, "title")?,
        html_url: string(map, "html_url")?,
        state: string(map, "state")?,
        merged: map.get("merged")?.as_bool()?,
        draft: optional_bool(map, "draft").ok()?,
        base: decode_branch(map.get("base")?)?,
        head: decode_branch(map.get("head")?)?,
        closed_at: optional_string(map, "closed_at", true).ok()?,
        merged_at: optional_string(map, "merged_at", true).ok()?,
        updated_at: optional_date(map, "updated_at").ok()?,
    })
}

/// `toForgejoChangeRequest`.
pub fn to_forgejo_change_request(raw: &ForgejoPullRequest) -> ChangeRequest {
    static WIP: OnceLock<Regex> = OnceLock::new();
    let wip = WIP.get_or_init(|| Regex::new(r"(?i)^(?:\[WIP\]|WIP:)").expect("valid regex"));
    let state = if raw.merged {
        ChangeRequestState::Merged
    } else if raw.state == "closed" {
        ChangeRequestState::Closed
    } else {
        ChangeRequestState::Open
    };
    ChangeRequest {
        provider: SourceControlProviderKind::Forgejo,
        number: raw.number,
        title: raw.title.clone(),
        url: raw.html_url.clone(),
        state,
        is_draft: Some(raw.draft.unwrap_or_else(|| wip.is_match(&raw.title))),
        base_ref_name: raw.base.ref_name.clone(),
        head_ref_name: raw.head.ref_name.clone(),
        closed_at: Some(raw.closed_at.clone()),
        merged_at: Some(raw.merged_at.clone()),
        updated_at: EOption(raw.updated_at),
        is_cross_repository: Some(match (&raw.head.repo, &raw.base.repo) {
            (Some(head), Some(base)) => head.full_name != base.full_name,
            _ => false,
        }),
        head_repository_name_with_owner: Some(raw.head.repo.as_ref().map(|r| r.full_name.clone())),
        head_repository_owner_login: Some(raw.head.repo.as_ref().map(|r| r.owner_login.clone())),
    }
}
