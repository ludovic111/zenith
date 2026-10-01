//! `BitbucketPullRequestProvider.test.ts`: optional reads recover from every failure but a rate
//! limit (the pure helpers are tested next to them in `src/bitbucket/provider.rs`).

#![allow(clippy::result_large_err)]

mod support_bitbucket;

use serde_json::json;
use support_bitbucket::*;
use zc_pullrequest::bitbucket::{BitbucketPullRequestApi, BitbucketPullRequestProvider};
use zc_pullrequest::provider::{ChangeRequestRef, PullRequestProviderApi};
use zc_pullrequest::ProviderFailureReason;
use zc_sourcecontrol::util::ManualClock;

/// The operation, and the path fragment its request carries.
const OPTIONAL_READS: [(&str, &str); 5] = [
    ("getMergeability", "/conflicts"),
    ("listChecks", "/statuses"),
    ("getRepositoryPermission", "/user/permissions/repositories"),
    ("listComments", "/comments"),
    ("listCommits", "/commits"),
];

fn pull_request() -> String {
    json!({
        "id": 1, "title": "Check polling", "state": "OPEN",
        "source": {"branch": {"name": "feature"}},
        "destination": {"branch": {"name": "main"}},
        "created_on": "2026-09-16T00:00:00Z",
        "updated_on": "2026-09-16T00:00:00Z",
        "links": {"html": {"href": "https://bitbucket.example.test/acme/web/pull-requests/1"}},
    })
    .to_string()
}

fn reference() -> ChangeRequestRef {
    ChangeRequestRef {
        cwd: "/repo".into(),
        repository: "acme/web".into(),
        host: "bitbucket.org".into(),
        number: 1,
    }
}

#[tokio::test]
async fn preserves_rate_limits_from_optional_reads_while_recovering_other_failures() {
    for (operation, fragment) in OPTIONAL_READS {
        for variant in ["response", "body read"] {
            for status in [429u16, 403] {
                let mock = MockRequester::new();
                let failing = fragment;
                mock.route(move |request| {
                    let path = request.url.split('?').next().unwrap_or_default();
                    if request.url.contains(failing) && !(failing == "/comments" && path.ends_with("/commits")) {
                        return Err(if variant == "response" {
                            response_error(status, Some(120_000))
                        } else {
                            body_read_error(status, Some(120_000))
                        });
                    }
                    if path.ends_with("/diffstat")
                        || path.ends_with("/conflicts")
                        || path.ends_with("/statuses")
                        || path.ends_with("/comments")
                        || path.ends_with("/commits")
                    {
                        return response(json!({"values": []}).to_string());
                    }
                    if path.starts_with("/user/permissions/repositories") {
                        return response(json!({"values": [{"permission": "write"}]}).to_string());
                    }
                    response(pull_request())
                });
                let provider = BitbucketPullRequestProvider::from_api(BitbucketPullRequestApi::with_requester(mock.clone(), ManualClock::new(0)));
                let result = if operation == "listComments" || operation == "listCommits" {
                    provider.get_change_request_activity(reference()).await.map(drop)
                } else {
                    provider.get_change_request(reference()).await.map(drop)
                };
                let label = format!("{operation} on {variant} errors, HTTP {status}");
                if status == 429 {
                    let error = result.expect_err(&label);
                    assert_eq!((error.reason, error.retry_at), (ProviderFailureReason::RateLimited, Some(120_000)), "{label}");
                } else {
                    assert!(result.is_ok(), "{label}: {result:?}");
                }
            }
        }
    }
}

#[tokio::test]
async fn names_the_operation_around_the_fact_bitbucket_stated() {
    let mock = MockRequester::new();
    mock.always(Err(response_error(401, None)));
    let provider = BitbucketPullRequestProvider::from_api(BitbucketPullRequestApi::with_requester(mock.clone(), ManualClock::new(0)));
    let error = provider.get_change_request(reference()).await.unwrap_err();
    assert_eq!(error.reason, ProviderFailureReason::Unauthenticated);
    assert_eq!(error.message(), "bitbucket failed in getChangeRequest: Bitbucket returned HTTP 401.");
    assert_eq!(error.cause.as_ref().and_then(|cause| cause.name()).as_deref(), Some("BitbucketResponseError"));
}
