//! `gitHubPullRequests.ts`: decoding `gh pr view|list --json …` output.

use serde_json::Value;
use zc_contracts::ChangeRequestState;

use crate::records::{decode_list, decode_one, NormalizedChangeRequest};
use crate::util::{js_trim, optional_bool, optional_date, optional_field, optional_string, positive_int, trim_optional_string, trimmed_non_empty};

/// `normalizeGitHubPullRequestState`.
fn normalize_state(state: Option<&str>, merged_at: Option<&str>) -> ChangeRequestState {
    let normalized = state.map(|s| js_trim(s).to_uppercase());
    if merged_at.is_some_and(|m| !js_trim(m).is_empty()) || normalized.as_deref() == Some("MERGED") {
        return ChangeRequestState::Merged;
    }
    if normalized.as_deref() == Some("CLOSED") {
        return ChangeRequestState::Closed;
    }
    ChangeRequestState::Open
}

/// `GitHubPullRequestSchema` decoding followed by `normalizeGitHubPullRequestRecord`. `None` when
/// the entry does not match the schema.
pub fn decode_github_pull_request(value: &Value) -> Option<NormalizedChangeRequest> {
    let object = value.as_object()?;
    let number = positive_int(object.get("number")?)?;
    let title = trimmed_non_empty(object.get("title")?)?;
    let url = trimmed_non_empty(object.get("url")?)?;
    let base_ref_name = trimmed_non_empty(object.get("baseRefName")?)?;
    let head_ref_name = trimmed_non_empty(object.get("headRefName")?)?;
    let state = optional_string(object, "state", true).ok()?;
    let is_draft = optional_bool(object, "isDraft").ok()?;
    let closed_at = optional_string(object, "closedAt", true).ok()?;
    let merged_at = optional_string(object, "mergedAt", true).ok()?;
    let updated_at = optional_date(object, "updatedAt").ok()?;
    let is_cross_repository = optional_bool(object, "isCrossRepository").ok()?;
    let head_repository = match optional_field(object, "headRepository", true).ok()? {
        None => None,
        Some(Value::Object(repository)) => Some((
            optional_string(repository, "nameWithOwner", true).ok()?,
            optional_string(repository, "name", true).ok()?,
        )),
        Some(_) => return None,
    };
    let owner_login = match optional_field(object, "headRepositoryOwner", true).ok()? {
        None => None,
        Some(Value::Object(owner)) => optional_string(owner, "login", true).ok()?,
        Some(_) => return None,
    };

    let explicit_name_with_owner = trim_optional_string(head_repository.as_ref().and_then(|(nwo, _)| nwo.as_deref()));
    let repository_name = trim_optional_string(head_repository.as_ref().and_then(|(_, name)| name.as_deref()));
    let head_repository_owner_login = trim_optional_string(owner_login.as_deref()).or_else(|| {
        explicit_name_with_owner
            .as_ref()
            .filter(|nwo| nwo.contains('/'))
            .and_then(|nwo| nwo.split('/').next().map(str::to_owned))
    });
    let head_repository_name_with_owner = explicit_name_with_owner
        .clone()
        .or_else(|| match (&head_repository_owner_login, &repository_name) {
            (Some(owner), Some(name)) => Some(format!("{owner}/{name}")),
            _ => None,
        });

    Some(NormalizedChangeRequest {
        number,
        title,
        url,
        base_ref_name,
        head_ref_name,
        state: normalize_state(state.as_deref(), merged_at.as_deref()),
        is_draft: (is_draft == Some(true)).then_some(true),
        closed_at: Some(closed_at),
        merged_at: Some(merged_at),
        updated_at,
        is_cross_repository,
        head_repository_name_with_owner: head_repository_name_with_owner.filter(|s| !s.is_empty()).map(Some),
        head_repository_owner_login: head_repository_owner_login.filter(|s| !s.is_empty()).map(Some),
    })
}

/// `decodeGitHubPullRequestListJson`.
pub fn decode_github_pull_request_list_json(raw: &str) -> Result<Vec<NormalizedChangeRequest>, String> {
    decode_list(raw, decode_github_pull_request)
}

/// `decodeGitHubPullRequestJson`.
pub fn decode_github_pull_request_json(raw: &str) -> Result<NormalizedChangeRequest, String> {
    decode_one(raw, decode_github_pull_request)
}
