//! References as the service keys them: the `PullRequestRef` part of every input, the credential
//! namespace a routed read is filed under, the cache key of a reference (`refCacheKey` /
//! `refOfCacheKey`) and its invalidation scope (`refScope`).

use serde_json::json;
use zc_contracts::{
    ProjectId, PullRequestActionInput, PullRequestCommentInput, PullRequestCommentUpdateInput, PullRequestDiffFileContentsInput, PullRequestDiffInput,
    PullRequestLabelChangeInput, PullRequestReactionInput, PullRequestRef, PullRequestReviewerRequestInput, PullRequestSetFilesViewedInput,
    PullRequestSubmitReviewInput, PullRequestThreadCommentsInput, PullRequestThreadReplyInput, PullRequestThreadResolutionInput, PullRequestUpdateInput,
};

use crate::util::lower;

/// An input that names a change request (`I extends PullRequestRef`).
pub trait RefInput {
    /// The `PullRequestRef` fields of the input.
    fn reference(&self) -> PullRequestRef;
}

impl RefInput for PullRequestRef {
    fn reference(&self) -> PullRequestRef {
        self.clone()
    }
}

macro_rules! ref_input {
    ($($input:ty),* $(,)?) => {
        $(impl RefInput for $input {
            fn reference(&self) -> PullRequestRef {
                PullRequestRef {
                    project_id: self.project_id.clone(),
                    host: self.host.clone(),
                    expected_account_id: self.expected_account_id.clone(),
                    allow_stale: self.allow_stale,
                    repository: self.repository.clone(),
                    number: self.number,
                }
            }
        })*
    };
}

ref_input!(
    PullRequestActionInput,
    PullRequestCommentInput,
    PullRequestCommentUpdateInput,
    PullRequestDiffFileContentsInput,
    PullRequestDiffInput,
    PullRequestLabelChangeInput,
    PullRequestReactionInput,
    PullRequestReviewerRequestInput,
    PullRequestSetFilesViewedInput,
    PullRequestSubmitReviewInput,
    PullRequestThreadCommentsInput,
    PullRequestThreadReplyInput,
    PullRequestThreadResolutionInput,
    PullRequestUpdateInput,
);

/// `CredentialRef`: a reference plus the credential namespace its cached reads are filed under
/// (internal: a client cannot choose it).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CredRef {
    pub reference: PullRequestRef,
    pub credential: Option<String>,
}

impl CredRef {
    pub(crate) fn plain(reference: PullRequestRef) -> Self {
        Self { reference, credential: None }
    }
}

/// `refScope(ref)`: the JSON text `[projectId, host, repository, number]` (host and repository
/// lower-cased, a missing host as `""`). Also a scope of the persisted read cache, so it is
/// spelled exactly like the TS `JSON.stringify`.
pub(crate) fn ref_scope(reference: &PullRequestRef) -> String {
    json!([
        reference.project_id.as_str(),
        reference.host.as_deref().map(lower).unwrap_or_default(),
        lower(&reference.repository),
        reference.number
    ])
    .to_string()
}

/// `refCacheKey(ref)`: what a cached read of a reference is keyed by. The epoch strands every
/// entry made before the reference (or its project) was last invalidated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RefKey {
    pub epoch: u64,
    pub project_id: String,
    pub host: Option<String>,
    pub repository: String,
    pub number: i64,
    pub expected_account_id: Option<String>,
    pub credential: Option<String>,
}

impl RefKey {
    pub(crate) fn new(epoch: u64, reference: &CredRef) -> Self {
        let r = &reference.reference;
        Self {
            epoch,
            project_id: r.project_id.as_str().to_owned(),
            host: r.host.as_deref().map(lower),
            repository: lower(&r.repository),
            number: r.number,
            expected_account_id: r.expected_account_id.clone(),
            credential: reference.credential.clone(),
        }
    }

    /// `refOfCacheKey(key)`: the reference back out of its key (host and repository as keyed,
    /// i.e. lower-cased; no `allowStale`).
    pub(crate) fn to_ref(&self) -> CredRef {
        CredRef {
            reference: PullRequestRef {
                project_id: ProjectId::new(self.project_id.clone()),
                host: self.host.clone(),
                expected_account_id: self.expected_account_id.clone(),
                allow_stale: None,
                repository: self.repository.clone(),
                number: self.number,
            },
            credential: self.credential.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spells_the_scope_like_json_stringify() {
        let reference = PullRequestRef {
            project_id: ProjectId::new("p1"),
            host: Some("GitHub.com".into()),
            expected_account_id: None,
            allow_stale: None,
            repository: "Acme/Web".into(),
            number: 7,
        };
        assert_eq!(ref_scope(&reference), r#"["p1","github.com","acme/web",7]"#);
        assert_eq!(ref_scope(&PullRequestRef { host: None, ..reference }), r#"["p1","","acme/web",7]"#);
    }
}
