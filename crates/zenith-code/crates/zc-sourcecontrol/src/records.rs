//! The normalized pull/merge request record every forge decoder produces
//! (`NormalizedGitHubPullRequestRecord`, `NormalizedGitLabMergeRequestRecord`, …): the
//! `ChangeRequest` fields with the presence rules of the TS object spreads.

use serde_json::{Map, Value};
use zc_contracts::{ChangeRequest, ChangeRequestState, DateTimeUtc, EOption, SourceControlProviderKind};

/// One normalized change request. `Option<Option<…>>` fields distinguish an absent key
/// (`None`) from `null` (`Some(None)`).
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedChangeRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub state: ChangeRequestState,
    /// Only ever `Some(true)` for the CLI decoders (`...(isDraft === true ? {isDraft: true} : {})`).
    pub is_draft: Option<bool>,
    pub closed_at: Option<Option<String>>,
    pub merged_at: Option<Option<String>>,
    pub updated_at: Option<DateTimeUtc>,
    pub is_cross_repository: Option<bool>,
    pub head_repository_name_with_owner: Option<Option<String>>,
    pub head_repository_owner_login: Option<Option<String>>,
}

impl NormalizedChangeRequest {
    /// The provider-neutral `ChangeRequest`, keeping every key that is present.
    pub fn to_change_request(&self, provider: SourceControlProviderKind) -> ChangeRequest {
        ChangeRequest {
            provider,
            number: self.number,
            title: self.title.clone(),
            url: self.url.clone(),
            base_ref_name: self.base_ref_name.clone(),
            head_ref_name: self.head_ref_name.clone(),
            state: self.state,
            is_draft: self.is_draft,
            closed_at: self.closed_at.clone(),
            merged_at: self.merged_at.clone(),
            updated_at: EOption(self.updated_at),
            is_cross_repository: self.is_cross_repository,
            head_repository_name_with_owner: self.head_repository_name_with_owner.clone(),
            head_repository_owner_login: self.head_repository_owner_login.clone(),
        }
    }

    /// The CLI summary object (`GitHubPullRequestSummary`, …) as TS returns it, with `updatedAt`
    /// as an ISO string when present. Used by golden comparisons and logs.
    pub fn to_summary_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("number".into(), Value::from(self.number));
        map.insert("title".into(), Value::from(self.title.clone()));
        map.insert("url".into(), Value::from(self.url.clone()));
        map.insert("baseRefName".into(), Value::from(self.base_ref_name.clone()));
        map.insert("headRefName".into(), Value::from(self.head_ref_name.clone()));
        map.insert("state".into(), Value::from(self.state.as_str()));
        if let Some(draft) = self.is_draft {
            map.insert("isDraft".into(), Value::Bool(draft));
        }
        let nullable = |value: &Option<String>| value.clone().map_or(Value::Null, Value::String);
        if let Some(closed) = &self.closed_at {
            map.insert("closedAt".into(), nullable(closed));
        }
        if let Some(merged) = &self.merged_at {
            map.insert("mergedAt".into(), nullable(merged));
        }
        if let Some(updated) = self.updated_at {
            map.insert("updatedAt".into(), Value::String(updated.to_iso_string()));
        }
        if let Some(cross) = self.is_cross_repository {
            map.insert("isCrossRepository".into(), Value::Bool(cross));
        }
        if let Some(owner) = &self.head_repository_name_with_owner {
            map.insert("headRepositoryNameWithOwner".into(), nullable(owner));
        }
        if let Some(login) = &self.head_repository_owner_login {
            map.insert("headRepositoryOwnerLogin".into(), nullable(login));
        }
        Value::Object(map)
    }
}

/// Decodes a JSON array, keeping the entries `decode_entry` accepts (`decode*ListJson`): a
/// malformed document fails, malformed entries are skipped.
pub fn decode_list<T>(raw: &str, decode_entry: impl Fn(&Value) -> Option<T>) -> Result<Vec<T>, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let Value::Array(entries) = value else {
        return Err("Expected an array".into());
    };
    Ok(entries.iter().filter_map(decode_entry).collect())
}

/// Decodes one JSON document with `decode` (`decode*Json`).
pub fn decode_one<T>(raw: &str, decode: impl Fn(&Value) -> Option<T>) -> Result<T, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    decode(&value).ok_or_else(|| "The value does not match the schema".into())
}
