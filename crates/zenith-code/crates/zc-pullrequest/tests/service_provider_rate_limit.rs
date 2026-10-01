//! `PullRequestProviderRateLimit.test.ts`: every pull request provider classifies its host's rate
//! limit as `rate-limited`, with the retry time where the host gave one.

use zc_contracts::SourceControlProviderKind;
use zc_pullrequest::azure::provider::azure_devops_provider_failure;
use zc_pullrequest::azure::AzureDevOpsPullRequestCliError;
use zc_pullrequest::bitbucket::provider::bitbucket_provider_failure;
use zc_pullrequest::bitbucket::BitbucketPullRequestApiError;
use zc_pullrequest::github::cli::GitHubPullRequestCliError;
use zc_pullrequest::github::provider::git_hub_provider_failure;
use zc_pullrequest::gitlab::provider::gitlab_provider_failure;
use zc_pullrequest::gitlab::GitLabPullRequestCliError;
use zc_pullrequest::ProviderFailureReason;
use zc_sourcecontrol::azure::{AzureDevOpsCliError, AzureDevOpsCliErrorKind};
use zc_sourcecontrol::bitbucket::BitbucketApiError;
use zc_sourcecontrol::errors::Cause;
use zc_sourcecontrol::github::{GitHubCliError, GitHubCliErrorKind};
use zc_sourcecontrol::gitlab::{GitLabCliError, GitLabCliErrorKind};
use zc_sourcecontrol::rate_limit::SourceControlRateLimitPausedError;

fn cause() -> Cause {
    Cause::message("redacted provider failure")
}

#[test]
fn classifies_rate_limits_from_every_pull_request_provider() {
    assert_eq!(
        git_hub_provider_failure(&GitHubPullRequestCliError::Cli(GitHubCliError::new(
            GitHubCliErrorKind::RateLimit { retry_at: None },
            "/repo",
            cause()
        ))),
        (ProviderFailureReason::RateLimited, None)
    );
    assert_eq!(
        gitlab_provider_failure(&GitLabPullRequestCliError::Cli(GitLabCliError::new(
            GitLabCliErrorKind::RateLimit,
            "/repo",
            cause()
        ))),
        ProviderFailureReason::RateLimited
    );
    assert_eq!(
        azure_devops_provider_failure(&AzureDevOpsPullRequestCliError::Cli(AzureDevOpsCliError {
            kind: AzureDevOpsCliErrorKind::RateLimit { argument_count: 1 },
            cwd: "/repo".into(),
            cause: cause(),
        })),
        ProviderFailureReason::RateLimited
    );
    assert_eq!(
        bitbucket_provider_failure(&BitbucketPullRequestApiError::Api(BitbucketApiError::Response {
            operation: "request",
            status: 429,
            response_body_length: 0,
            retry_at: Some(120_000),
        })),
        (ProviderFailureReason::RateLimited, Some(120_000))
    );
}

#[test]
fn keeps_githubs_exact_retry_time() {
    assert_eq!(
        git_hub_provider_failure(&GitHubPullRequestCliError::RateLimitPaused(SourceControlRateLimitPausedError {
            provider: SourceControlProviderKind::Github,
            host: "github.com".into(),
            retry_at: 1_786_802_400_000,
        })),
        (ProviderFailureReason::RateLimited, Some(1_786_802_400_000))
    );
}
