//! Wire types of the VCS, git and review RPCs (`packages/contracts/src/git.ts`, `vcs.ts`,
//! `review.ts`, `sourceControl.ts`), hand-written until `zc-contracts` (WP-01) lands.
//!
//! Each struct names the contract schema it encodes. They follow plan §1.5:
//! - `Schema.optional(X)` / `optionalKey(X)` → `Option<T>`, never serialized as `null`;
//! - `Schema.NullOr(X)` → `Option<T>`, always serialized (`null` when absent);
//! - `optional(NullOr(X))` → [`Nullable`]: absent, `null` or a value;
//! - `Schema.Option(X)` → [`TaggedOption`] (`{"_tag":"Some","value":…}` / `{"_tag":"None"}`);
//! - `TrimmedNonEmptyString` inputs are trimmed (and must not be empty) on decode.
//!
//! **Swapping in the generated types:** each type here should become `pub use
//! zc_contracts::<Name>;`. Field names and serde attributes are already the wire ones, so only
//! the Rust field types of branded strings may change. Struct field order is the schema's
//! declaration order (cosmetic on the wire).

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Serde helpers for `TrimmedNonEmptyString` (`baseSchemas.ts`): trimmed on decode, rejected
/// when empty.
pub mod trimmed {
    use super::*;

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(serde::de::Error::custom("Expected a non-empty string after trimming"));
        }
        Ok(trimmed.to_owned())
    }

    /// `Schema.optional(TrimmedNonEmptyString)`.
    pub mod option {
        use super::*;

        pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
            let raw = Option::<String>::deserialize(deserializer)?;
            match raw {
                None => Ok(None),
                Some(raw) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        Err(serde::de::Error::custom("Expected a non-empty string after trimming"))
                    } else {
                        Ok(Some(trimmed.to_owned()))
                    }
                }
            }
        }
    }
}

/// `Schema.NonEmptyString` (not trimmed).
mod non_empty {
    use super::*;

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
        let raw = String::deserialize(deserializer)?;
        if raw.is_empty() {
            return Err(serde::de::Error::custom("Expected a non-empty string"));
        }
        Ok(raw)
    }

    pub mod nullable {
        use super::*;

        pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
            match Option::<String>::deserialize(deserializer)? {
                Some(raw) if raw.is_empty() => Err(serde::de::Error::custom("Expected a non-empty string")),
                other => Ok(other),
            }
        }
    }
}

/// `Schema.optional(Schema.NullOr(X))`: absent, `null`, or a value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Nullable<T> {
    #[default]
    Absent,
    Null,
    Value(T),
}

impl<T> Nullable<T> {
    pub fn is_absent(&self) -> bool {
        matches!(self, Self::Absent)
    }
}

impl<T: Serialize> Serialize for Nullable<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Absent | Self::Null => serializer.serialize_none(),
            Self::Value(value) => value.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Nullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            None => Self::Null,
            Some(value) => Self::Value(value),
        })
    }
}

/// `Schema.Option(X)` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum TaggedOption<T> {
    None,
    Some { value: T },
}

impl<T> From<Option<T>> for TaggedOption<T> {
    fn from(value: Option<T>) -> Self {
        match value {
            None => Self::None,
            Some(value) => Self::Some { value },
        }
    }
}

// ---------------------------------------------------------------------------------------------
// vcs.ts
// ---------------------------------------------------------------------------------------------

/// `VcsDriverKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsDriverKind {
    Git,
    Jj,
    Unknown,
}

impl VcsDriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Jj => "jj",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for VcsDriverKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `VcsDriverKind | "auto"`, the requested kind of `VcsDriverResolveInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestedVcsKind {
    Auto,
    Kind(VcsDriverKind),
}

impl RequestedVcsKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Kind(kind) => kind.as_str(),
        }
    }
}

/// `VcsFreshnessSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VcsFreshnessSource {
    LiveLocal,
    CachedLocal,
    CachedRemote,
    ExplicitRemote,
}

/// `VcsFreshness`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsFreshness {
    pub source: VcsFreshnessSource,
    /// `DateTimeUtc`: ISO with milliseconds.
    pub observed_at: String,
    pub expires_at: TaggedOption<String>,
}

impl VcsFreshness {
    /// `nowFreshness` of `GitVcsDriver.ts`.
    pub fn live_local_now() -> Self {
        Self {
            source: VcsFreshnessSource::LiveLocal,
            observed_at: zc_core::time::now_iso(),
            expires_at: TaggedOption::None,
        }
    }
}

/// `VcsDriverCapabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsDriverCapabilities {
    pub kind: VcsDriverKind,
    pub supports_worktrees: bool,
    pub supports_bookmarks: bool,
    pub supports_atomic_snapshot: bool,
    pub supports_push_default_remote: bool,
    /// `"native" | "git-compatible-fallback"`.
    pub ignore_classifier: String,
}

/// `VcsRepositoryIdentity`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsRepositoryIdentity {
    pub kind: VcsDriverKind,
    pub root_path: String,
    pub metadata_path: Option<String>,
    pub freshness: VcsFreshness,
}

/// `VcsListWorkspaceFilesResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsListWorkspaceFilesResult {
    pub paths: Vec<String>,
    pub truncated: bool,
    pub freshness: VcsFreshness,
}

/// `VcsRemote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsRemote {
    pub name: String,
    pub url: String,
    pub push_url: TaggedOption<String>,
    pub is_primary: bool,
}

/// `VcsListRemotesResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsListRemotesResult {
    pub remotes: Vec<VcsRemote>,
    pub freshness: VcsFreshness,
}

// ---------------------------------------------------------------------------------------------
// sourceControl.ts
// ---------------------------------------------------------------------------------------------

/// `SourceControlProviderKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceControlProviderKind {
    Github,
    Gitlab,
    Forgejo,
    AzureDevops,
    Bitbucket,
    Unknown,
}

/// `SourceControlProviderInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceControlProviderInfo {
    pub kind: SourceControlProviderKind,
    pub name: String,
    pub base_url: String,
}

// ---------------------------------------------------------------------------------------------
// git.ts: inputs
// ---------------------------------------------------------------------------------------------

/// `VcsStatusInput` (also `VcsPullInput`, same shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsStatusInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
}

/// `VcsPullInput`.
pub type VcsPullInput = VcsStatusInput;

/// `refKind` of `VcsListRefsInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VcsRefKind {
    All,
    Local,
    Remote,
}

/// `VcsListRefsInput`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsListRefsInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    /// Max 256 characters.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "trimmed::option::deserialize")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_matching_remote_refs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_kind: Option<VcsRefKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh: Option<bool>,
    /// `1..=200`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl VcsListRefsInput {
    /// The refinements serde cannot express (`isMaxLength(256)`, `PositiveInt ≤ 200`).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(query) = &self.query {
            if query.encode_utf16().count() > 256 {
                return Err("Expected a value with a length of at most 256".into());
            }
        }
        if let Some(limit) = self.limit {
            if !(1..=200).contains(&limit) {
                return Err("Expected a positive integer of at most 200".into());
            }
        }
        Ok(())
    }
}

/// `VcsCreateWorktreeInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsCreateWorktreeInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub ref_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "trimmed::option::deserialize")]
    pub new_ref_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "trimmed::option::deserialize")]
    pub base_ref_name: Option<String>,
    /// `NullOr`: always present on the wire.
    #[serde(default, deserialize_with = "trimmed::option::deserialize")]
    pub path: Option<String>,
}

/// `VcsRemoveWorktreeInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsRemoveWorktreeInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force: Option<bool>,
}

/// `VcsCreateRefInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsCreateRefInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub ref_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switch_ref: Option<bool>,
}

/// `VcsSwitchRefInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsSwitchRefInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub ref_name: String,
}

/// `VcsInitInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsInitInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<VcsDriverKind>,
}

// ---------------------------------------------------------------------------------------------
// git.ts: results
// ---------------------------------------------------------------------------------------------

/// `VcsRef`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsRef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_remote: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_name: Option<String>,
    pub current: bool,
    pub is_default: bool,
    pub worktree_path: Option<String>,
}

impl VcsRef {
    pub fn is_remote(&self) -> bool {
        self.is_remote == Some(true)
    }
}

/// `VcsListRefsResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsListRefsResult {
    pub refs: Vec<VcsRef>,
    pub is_repo: bool,
    pub has_primary_remote: bool,
    pub next_cursor: Option<u64>,
    pub total_count: u64,
}

impl VcsListRefsResult {
    pub fn non_repository() -> Self {
        Self {
            refs: Vec::new(),
            is_repo: false,
            has_primary_remote: false,
            next_cursor: None,
            total_count: 0,
        }
    }
}

/// `VcsWorktree`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsWorktree {
    pub path: String,
    pub ref_name: String,
}

/// `VcsCreateWorktreeResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsCreateWorktreeResult {
    pub worktree: VcsWorktree,
}

/// `VcsCreateRefResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsCreateRefResult {
    pub ref_name: String,
}

/// `VcsSwitchRefResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsSwitchRefResult {
    pub ref_name: Option<String>,
}

/// `status` of `VcsPullResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VcsPullStatus {
    Pulled,
    SkippedUpToDate,
}

/// `VcsPullResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsPullResult {
    pub status: VcsPullStatus,
    pub ref_name: String,
    pub upstream_ref: Option<String>,
}

/// One entry of `workingTree.files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingTreeFile {
    pub path: String,
    pub insertions: u64,
    pub deletions: u64,
}

/// `workingTree` of `VcsStatusLocalResult`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkingTree {
    pub files: Vec<WorkingTreeFile>,
    pub insertions: u64,
    pub deletions: u64,
}

/// `VcsStatusLocalResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsStatusLocalResult {
    pub is_repo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_control_provider: Option<SourceControlProviderInfo>,
    pub has_primary_remote: bool,
    pub is_default_ref: bool,
    pub ref_name: Option<String>,
    pub has_working_tree_changes: bool,
    pub working_tree: WorkingTree,
}

impl VcsStatusLocalResult {
    /// `nonRepositoryLocalStatus` of `GitWorkflowService.ts`.
    pub fn non_repository() -> Self {
        Self {
            is_repo: false,
            source_control_provider: None,
            has_primary_remote: false,
            is_default_ref: false,
            ref_name: None,
            has_working_tree_changes: false,
            working_tree: WorkingTree::default(),
        }
    }
}

/// `VcsStatusChangeRequest` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeRequestState {
    Open,
    Closed,
    Merged,
}

/// `VcsStatusChangeRequest` (`NonNullable<VcsStatusResult["pr"]>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsStatusChangeRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub base_ref: String,
    pub head_ref: String,
    pub state: ChangeRequestState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_draft: Option<bool>,
    #[serde(default, skip_serializing_if = "Nullable::is_absent")]
    pub updated_at: Nullable<String>,
}

/// `VcsStatusRemoteResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VcsStatusRemoteResult {
    pub has_upstream: bool,
    pub ahead_count: u64,
    pub behind_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ahead_of_default_count: Option<u64>,
    pub pr: Option<VcsStatusChangeRequest>,
}

impl VcsStatusRemoteResult {
    /// `EMPTY_GIT_STATUS_REMOTE` of `shared/git.ts`.
    pub fn empty() -> Self {
        Self {
            has_upstream: false,
            ahead_count: 0,
            behind_count: 0,
            ahead_of_default_count: Some(0),
            pr: None,
        }
    }
}

/// `VcsStatusResult`: the local fields then the remote ones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VcsStatusResult {
    #[serde(flatten)]
    pub local: VcsStatusLocalResult,
    #[serde(flatten)]
    pub remote: VcsStatusRemoteResult,
}

impl VcsStatusResult {
    /// `mergeGitStatusParts`.
    pub fn merge(local: VcsStatusLocalResult, remote: Option<VcsStatusRemoteResult>) -> Self {
        Self {
            local,
            remote: remote.unwrap_or_else(VcsStatusRemoteResult::empty),
        }
    }

    /// `nonRepositoryStatus` of `GitWorkflowService.ts`.
    pub fn non_repository() -> Self {
        Self::merge(VcsStatusLocalResult::non_repository(), None)
    }
}

/// `VcsStatusStreamEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "_tag")]
pub enum VcsStatusStreamEvent {
    #[serde(rename = "snapshot")]
    Snapshot {
        local: VcsStatusLocalResult,
        remote: Option<VcsStatusRemoteResult>,
    },
    #[serde(rename = "localUpdated")]
    LocalUpdated { local: VcsStatusLocalResult },
    #[serde(rename = "remoteUpdated")]
    RemoteUpdated { remote: Option<VcsStatusRemoteResult> },
}

// ---------------------------------------------------------------------------------------------
// review.ts
// ---------------------------------------------------------------------------------------------

/// `ReviewDiffPreviewSourceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDiffPreviewSourceKind {
    WorkingTree,
    BranchRange,
}

/// `file` of `ReviewDiffPreviewInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffPreviewFile {
    #[serde(deserialize_with = "non_empty::deserialize")]
    pub path: String,
    #[serde(default, deserialize_with = "non_empty::nullable::deserialize")]
    pub previous_path: Option<String>,
    pub source_kind: ReviewDiffPreviewSourceKind,
}

/// `ReviewDiffPreviewInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffPreviewInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "trimmed::option::deserialize")]
    pub base_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_whitespace: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<ReviewDiffPreviewFile>,
}

/// `ReviewDiffFileStat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffFileStat {
    pub path: String,
    pub previous_path: Option<String>,
    pub additions: u64,
    pub deletions: u64,
}

/// `ReviewDiffPreviewSource`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffPreviewSource {
    pub id: String,
    pub kind: ReviewDiffPreviewSourceKind,
    pub title: String,
    pub base_ref: Option<String>,
    pub head_ref: Option<String>,
    pub diff: String,
    pub diff_hash: String,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<ReviewDiffFileStat>>,
}

/// `ReviewDiffPreviewResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffPreviewResult {
    pub cwd: String,
    /// `DateTimeUtc`.
    pub generated_at: String,
    pub sources: Vec<ReviewDiffPreviewSource>,
}

impl ReviewDiffPreviewResult {
    pub fn empty(cwd: &str) -> Self {
        Self {
            cwd: cwd.to_owned(),
            generated_at: zc_core::time::now_iso(),
            sources: Vec::new(),
        }
    }
}

/// `changeType` of `ReviewDiffFileContentsInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewDiffChangeType {
    Change,
    RenamePure,
    RenameChanged,
    New,
    Deleted,
}

/// `ReviewDiffFileContentsInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffFileContentsInput {
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub cwd: String,
    pub source_kind: ReviewDiffPreviewSourceKind,
    pub change_type: ReviewDiffChangeType,
    #[serde(default, deserialize_with = "trimmed::option::deserialize")]
    pub base_ref: Option<String>,
    #[serde(default, deserialize_with = "trimmed::option::deserialize")]
    pub head_ref: Option<String>,
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub old_path: String,
    #[serde(deserialize_with = "trimmed::deserialize")]
    pub new_path: String,
}

/// `ReviewDiffFileContentsResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewDiffFileContentsResult {
    pub old_contents: String,
    pub new_contents: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_result_merges_local_then_remote() {
        let merged = VcsStatusResult::non_repository();
        assert_eq!(
            serde_json::to_value(&merged).unwrap(),
            json!({
                "isRepo": false,
                "hasPrimaryRemote": false,
                "isDefaultRef": false,
                "refName": null,
                "hasWorkingTreeChanges": false,
                "workingTree": {"files": [], "insertions": 0, "deletions": 0},
                "hasUpstream": false,
                "aheadCount": 0,
                "behindCount": 0,
                "aheadOfDefaultCount": 0,
                "pr": null
            })
        );
        let decoded: VcsStatusResult = serde_json::from_value(serde_json::to_value(&merged).unwrap()).unwrap();
        assert_eq!(decoded, merged);
    }

    #[test]
    fn stream_events_are_tagged() {
        let event = VcsStatusStreamEvent::RemoteUpdated { remote: None };
        assert_eq!(serde_json::to_value(&event).unwrap(), json!({"_tag": "remoteUpdated", "remote": null}));
    }

    #[test]
    fn inputs_are_trimmed_and_non_empty() {
        let input: VcsStatusInput = serde_json::from_value(json!({"cwd": "  /repo "})).unwrap();
        assert_eq!(input.cwd, "/repo");
        assert!(serde_json::from_value::<VcsStatusInput>(json!({"cwd": "   "})).is_err());
        let refs: VcsListRefsInput = serde_json::from_value(json!({"cwd": "/r", "limit": 500})).unwrap();
        assert!(refs.validate().is_err());
    }

    #[test]
    fn nullable_and_tagged_option_encode_like_effect() {
        let pr = VcsStatusChangeRequest {
            number: 1,
            title: "t".into(),
            url: "u".into(),
            base_ref: "main".into(),
            head_ref: "f".into(),
            state: ChangeRequestState::Open,
            is_draft: None,
            updated_at: Nullable::Null,
        };
        let value = serde_json::to_value(&pr).unwrap();
        assert_eq!(value["updatedAt"], json!(null));
        let absent = VcsStatusChangeRequest {
            updated_at: Nullable::Absent,
            ..pr
        };
        assert!(serde_json::to_value(&absent).unwrap().get("updatedAt").is_none());
        assert_eq!(serde_json::to_value(TaggedOption::<String>::None).unwrap(), json!({"_tag": "None"}));
        assert_eq!(
            serde_json::to_value(TaggedOption::from(Some("x".to_owned()))).unwrap(),
            json!({"_tag": "Some", "value": "x"})
        );
    }
}
