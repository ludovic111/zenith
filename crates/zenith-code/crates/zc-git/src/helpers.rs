//! The pure functions of `git/GitManager.ts` (head matching, repository names from remote and
//! pull request URLs, commit message sanitizing, toast text, backoff) and the two helpers it
//! takes from `@t3tools/shared` (`resolveAutoFeatureBranchName`,
//! `getChangeRequestTerminologyForKind`).

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use zc_textgen::js::{len16, slice_head16, trim, trim_end};
use zc_textgen::utils::sanitize_feature_branch_name;
use zc_vcs::shared_git::normalize_git_remote_url;

use crate::types::{BranchHeadContext, PullRequestInfo};

pub const SHORT_SHA_LENGTH: usize = 7;
pub const TOAST_DESCRIPTION_MAX: usize = 72;
pub const MAX_PROGRESS_TEXT_LENGTH: usize = 500;

/// `PR_LOOKUP_CACHE_TTL`: an open PR is rechecked every minute (the settlement sweep cadence).
pub const PR_LOOKUP_CACHE_TTL: Duration = Duration::from_secs(60);
/// `PR_LOOKUP_NO_OPEN_PR_CACHE_TTL`: answers without an open PR wait five minutes.
pub const PR_LOOKUP_NO_OPEN_PR_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
pub const PR_LOOKUP_FAILURE_BASE_TTL: Duration = Duration::from_secs(20);
pub const PR_LOOKUP_FAILURE_MAX_TTL: Duration = Duration::from_secs(15 * 60);
pub const PR_LOOKUP_CACHE_CAPACITY: usize = 2_048;

/// `prLookupFailureTtl(consecutiveFailures)`: 20 s doubling per failure, capped at 15 min.
pub fn pr_lookup_failure_ttl(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(30);
    let backoff = PR_LOOKUP_FAILURE_BASE_TTL.as_millis() as u64 * (1u64 << exponent);
    Duration::from_millis(backoff).min(PR_LOOKUP_FAILURE_MAX_TTL)
}

/// `pullRequestRepositoryKey(url)`: the normalized repository of a change request URL.
pub fn pull_request_repository_key(value: &str) -> Option<String> {
    static PATH: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(.*)(?:/pull/|/-/merge_requests/|/pull-requests/|/pullrequest/)\d+(?:/.*)?$").expect("valid regex"));
    let mut url = url::Url::parse(value).ok()?;
    let path = url.path().to_owned();
    let repository = PATH.captures(&path)?.get(1)?.as_str().to_owned();
    url.set_path(&repository);
    url.set_query(None);
    url.set_fragment(None);
    Some(normalize_git_remote_url(url.as_str()))
}

/// `parseRepositoryNameFromPullRequestUrl`: `repo` of `https://host/owner/repo/pull/N`.
pub fn parse_repository_name_from_pull_request_url(url: &str) -> Option<String> {
    static PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://[^/]+/[^/]+/([^/]+)/pull/\d+(?:/.*)?$").expect("valid regex"));
    let name = PATTERN.captures(trim(url))?.get(1).map(|m| trim(m.as_str()).to_owned())?;
    (!name.is_empty()).then_some(name)
}

/// `normalizeOptionalString`.
pub fn normalize_optional_string(value: Option<&str>) -> Option<String> {
    value.map(trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

/// `resolvePullRequestHeadRepositoryNameWithOwner` (also `resolveHeadRepositoryNameWithOwner`,
/// the same rules on a resolved pull request).
pub fn resolve_head_repository_name_with_owner(
    url: &str,
    is_cross_repository: Option<bool>,
    head_repository_name_with_owner: Option<&str>,
    head_repository_owner_login: Option<&str>,
) -> Option<String> {
    if let Some(explicit) = normalize_optional_string(head_repository_name_with_owner) {
        return Some(explicit);
    }
    if is_cross_repository != Some(true) {
        return None;
    }
    let owner = normalize_optional_string(head_repository_owner_login)?;
    let repository = parse_repository_name_from_pull_request_url(url)?;
    Some(format!("{owner}/{repository}"))
}

/// `parseRepositoryNameWithOwnerFromRemoteUrl(url, providerKind?)`.
pub fn parse_repository_name_with_owner_from_remote_url(url: Option<&str>, provider_kind: Option<&str>) -> Option<String> {
    static PATTERN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)^(?:[^@/\s]+@[^:/\s]+:|(?:ssh|https?|git)://[^/]+/)((?:[^/\s]+/)+[^/\s]+?)(?:\.git)?/?$").expect("valid regex"));
    static HTTP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^https?://").expect("valid regex"));
    let trimmed = trim(url.unwrap_or(""));
    if trimmed.is_empty() {
        return None;
    }
    let name = PATTERN
        .captures(trimmed)
        .and_then(|captures| captures.get(1))
        .map(|m| trim(m.as_str()).to_owned())
        .unwrap_or_default();
    if provider_kind == Some("forgejo") && HTTP.is_match(trimmed) {
        if name.is_empty() {
            return None;
        }
        let segments: Vec<&str> = name.split('/').collect();
        return Some(segments[segments.len().saturating_sub(2)..].join("/"));
    }
    (!name.is_empty()).then_some(name)
}

/// `parseRepositoryOwnerLogin`: the first path segment.
pub fn parse_repository_owner_login(name_with_owner: Option<&str>) -> Option<String> {
    let trimmed = trim(name_with_owner.unwrap_or(""));
    if trimmed.is_empty() {
        return None;
    }
    let owner = trim(trimmed.split('/').next().unwrap_or(""));
    (!owner.is_empty()).then(|| owner.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HeadIdentity {
    repository_name_with_owner: Option<String>,
    owner_login: Option<String>,
}

fn lower(value: Option<String>) -> Option<String> {
    value.map(|value| value.to_lowercase())
}

/// `matchesBranchHeadContext(pr, headContext)`.
pub fn matches_branch_head_context(pr: &PullRequestInfo, head: &BranchHeadContext) -> bool {
    if pr.head_ref_name != head.head_branch {
        return false;
    }
    let expected_repository = lower(normalize_optional_string(head.head_repository_name_with_owner.as_deref()));
    let expected = HeadIdentity {
        owner_login: lower(normalize_optional_string(head.head_repository_owner_login.as_deref()))
            .or_else(|| parse_repository_owner_login(expected_repository.as_deref())),
        repository_name_with_owner: expected_repository,
    };
    let pr_repository = lower(resolve_head_repository_name_with_owner(
        &pr.url,
        pr.is_cross_repository,
        pr.head_repository_name_with_owner.as_deref(),
        pr.head_repository_owner_login.as_deref(),
    ));
    let actual = HeadIdentity {
        owner_login: lower(normalize_optional_string(pr.head_repository_owner_login.as_deref()))
            .or_else(|| parse_repository_owner_login(pr_repository.as_deref())),
        repository_name_with_owner: pr_repository,
    };

    if let Some(expected_repository) = &expected.repository_name_with_owner {
        if let Some(actual_repository) = &actual.repository_name_with_owner {
            if expected_repository != actual_repository {
                return false;
            }
        }
        if let (Some(a), Some(b)) = (&expected.owner_login, &actual.owner_login) {
            if a != b {
                return false;
            }
        }
    }
    if let (Some(a), Some(b)) = (&expected.owner_login, &actual.owner_login) {
        if a != b {
            return false;
        }
    }

    let expected_known = expected.repository_name_with_owner.is_some() || expected.owner_login.is_some();
    let actual_known = actual.repository_name_with_owner.is_some() || actual.owner_login.is_some();
    if head.is_cross_repository {
        if pr.is_cross_repository == Some(false) {
            return false;
        }
        if expected_known && !actual_known {
            return false;
        }
        return true;
    }
    if pr.is_cross_repository == Some(true) && (!expected_known || !actual_known) {
        return false;
    }
    true
}

/// `limitContext(value, maxChars)`.
pub fn limit_context(value: &str, max_chars: usize) -> String {
    if len16(value) <= max_chars {
        return value.to_owned();
    }
    format!("{}\n\n[truncated]", slice_head16(value, max_chars))
}

/// `shortenSha`.
pub fn shorten_sha(sha: Option<&str>) -> Option<String> {
    sha.filter(|sha| !sha.is_empty()).map(|sha| slice_head16(sha, SHORT_SHA_LENGTH).to_owned())
}

/// `truncateText(value, maxLength = 72)`.
pub fn truncate_text(value: Option<&str>, max_length: usize) -> Option<String> {
    let value = value.filter(|value| !value.is_empty())?;
    if len16(value) <= max_length {
        return Some(value.to_owned());
    }
    if max_length <= 3 {
        return Some(slice_head16("...", max_length).to_owned());
    }
    Some(format!("{}...", trim_end(slice_head16(value, max_length - 3))))
}

/// `sanitizeCommitMessage`: the first subject line without trailing periods, at most 72
/// characters (or `Update project files`), and the trimmed body.
pub fn sanitize_commit_message(subject: &str, body: &str) -> (String, String) {
    static PERIODS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.]+$").expect("valid regex"));
    static LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\r?\n").expect("valid regex"));
    let first = LINES.split(trim(subject)).next().map(trim).unwrap_or("");
    let subject = trim(&PERIODS.replace(first, "")).to_owned();
    let safe = if subject.is_empty() {
        "Update project files".to_owned()
    } else {
        trim_end(slice_head16(&subject, 72)).to_owned()
    };
    (safe, trim(body).to_owned())
}

/// `sanitizeProgressText`: trimmed, at most 500 characters, `None` when empty.
pub fn sanitize_progress_text(value: &str) -> Option<String> {
    let trimmed = trim(value);
    if trimmed.is_empty() {
        return None;
    }
    if len16(trimmed) <= MAX_PROGRESS_TEXT_LENGTH {
        return Some(trimmed.to_owned());
    }
    Some(trim_end(slice_head16(trimmed, MAX_PROGRESS_TEXT_LENGTH)).to_owned())
}

/// `formatCommitMessage(subject, body)`.
pub fn format_commit_message(subject: &str, body: &str) -> String {
    let body = trim(body);
    if body.is_empty() {
        subject.to_owned()
    } else {
        format!("{subject}\n\n{body}")
    }
}

/// `parseCustomCommitMessage(raw)`: the first line is the subject, the rest the body.
pub fn parse_custom_commit_message(raw: &str) -> Option<(String, String)> {
    let normalized = raw.replace("\r\n", "\n");
    let normalized = trim(&normalized);
    if normalized.is_empty() {
        return None;
    }
    let mut lines = normalized.split('\n');
    let subject = trim(lines.next().unwrap_or("")).to_owned();
    if subject.is_empty() {
        return None;
    }
    let body = trim(&lines.collect::<Vec<_>>().join("\n")).to_owned();
    Some((subject, body))
}

/// `GITHUB_HEAD_BRANCH_PROBE_LIMIT`: GitHub probes ask for a full page and let
/// [`matches_branch_head_context`] pick the head.
pub const GITHUB_HEAD_BRANCH_PROBE_LIMIT: u32 = 100;

/// `probeableHeadSelectors(providerKind, headSelectors)`: `gh pr list --head` takes a bare
/// branch name only, so GitHub skips `owner:branch` selectors.
pub fn probeable_head_selectors(provider_kind: &str, head_selectors: &[String]) -> Vec<String> {
    if provider_kind == "github" {
        head_selectors.iter().filter(|selector| !selector.contains(':')).cloned().collect()
    } else {
        head_selectors.to_vec()
    }
}

/// `appendUnique(values, next)`.
pub fn append_unique(values: &mut Vec<String>, next: Option<&str>) {
    let trimmed = trim(next.unwrap_or(""));
    if trimmed.is_empty() || values.iter().any(|value| value == trimmed) {
        return;
    }
    values.push(trimmed.to_owned());
}

/// `normalizePullRequestReference`: `#42` → `42`.
pub fn normalize_pull_request_reference(reference: &str) -> String {
    static HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^#(\d+)$").expect("valid regex"));
    let trimmed = trim(reference);
    HASH.captures(trimmed)
        .and_then(|captures| captures.get(1))
        .map(|m| m.as_str().to_owned())
        .unwrap_or_else(|| trimmed.to_owned())
}

/// `resolvePullRequestWorktreeLocalBranchName`: a fork PR gets `t3code/pr-<n>/<head>`.
pub fn resolve_pull_request_worktree_local_branch_name(number: i64, head_branch: &str, is_cross_repository: Option<bool>) -> String {
    if is_cross_repository != Some(true) {
        return head_branch.to_owned();
    }
    let sanitized = zc_textgen::utils::sanitize_branch_fragment(head_branch);
    let sanitized = trim(&sanitized);
    let suffix = if sanitized.is_empty() { "head" } else { sanitized };
    format!("t3code/pr-{number}/{suffix}")
}

const AUTO_FEATURE_BRANCH_FALLBACK: &str = "feature/update";

/// `resolveAutoFeatureBranchName(existingBranchNames, preferredBranch?)`.
pub fn resolve_auto_feature_branch_name(existing: &[String], preferred: Option<&str>) -> String {
    let preferred = preferred.map(trim).filter(|preferred| !preferred.is_empty());
    let base = sanitize_feature_branch_name(preferred.unwrap_or(AUTO_FEATURE_BRANCH_FALLBACK));
    let existing: HashSet<String> = existing.iter().map(|name| name.to_lowercase()).collect();
    if !existing.contains(&base) {
        return base;
    }
    let mut suffix = 2;
    while existing.contains(&format!("{base}-{suffix}")) {
        suffix += 1;
    }
    format!("{base}-{suffix}")
}

/// `ChangeRequestTerminology`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeRequestTerminology {
    pub short_label: &'static str,
    pub singular: &'static str,
}

/// `getChangeRequestTerminologyForKind(kind)`.
pub fn change_request_terminology(kind: &str) -> ChangeRequestTerminology {
    match kind {
        "gitlab" => ChangeRequestTerminology {
            short_label: "MR",
            singular: "merge request",
        },
        "unknown" => ChangeRequestTerminology {
            short_label: "change request",
            singular: "change request",
        },
        _ => ChangeRequestTerminology {
            short_label: "PR",
            singular: "pull request",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backs_off_failed_lookups_past_the_healthy_cadence() {
        // GitManager.test.ts: "backs off repeated PR lookup failures past the healthy refresh cadence"
        assert_eq!(pr_lookup_failure_ttl(0), Duration::from_secs(20));
        assert_eq!(pr_lookup_failure_ttl(1), Duration::from_secs(20));
        assert_eq!(pr_lookup_failure_ttl(2), Duration::from_secs(40));
        assert_eq!(pr_lookup_failure_ttl(3), Duration::from_secs(80));
        assert!(pr_lookup_failure_ttl(3) > PR_LOOKUP_CACHE_TTL);
        assert_eq!(pr_lookup_failure_ttl(10), PR_LOOKUP_FAILURE_MAX_TTL);
        assert_eq!(pr_lookup_failure_ttl(100), PR_LOOKUP_FAILURE_MAX_TTL);
    }

    #[test]
    fn repository_keys_from_pull_request_urls() {
        assert_eq!(
            pull_request_repository_key("https://github.com/Octo/Repo/pull/12").as_deref(),
            Some("github.com/octo/repo")
        );
        assert_eq!(
            pull_request_repository_key("https://gitlab.com/group/sub/proj/-/merge_requests/3/diffs").as_deref(),
            Some("gitlab.com/group/sub/proj")
        );
        assert_eq!(pull_request_repository_key("not a url"), None);
        assert_eq!(pull_request_repository_key("https://github.com/octo/repo/issues/1"), None);
    }

    #[test]
    fn repository_names_from_remote_urls() {
        assert_eq!(
            parse_repository_name_with_owner_from_remote_url(Some("git@github.com:octocat/demo.git"), None).as_deref(),
            Some("octocat/demo")
        );
        assert_eq!(
            parse_repository_name_with_owner_from_remote_url(Some("https://gitlab.com/group/sub/proj.git"), None).as_deref(),
            Some("group/sub/proj")
        );
        assert_eq!(
            parse_repository_name_with_owner_from_remote_url(Some("https://forge.example.test/git/owner/repo.git"), Some("forgejo")).as_deref(),
            Some("owner/repo")
        );
        assert_eq!(parse_repository_name_with_owner_from_remote_url(Some("/tmp/bare.git"), None), None);
        assert_eq!(parse_repository_name_with_owner_from_remote_url(None, None), None);
        assert_eq!(parse_repository_owner_login(Some("group/sub/proj")).as_deref(), Some("group"));
    }

    #[test]
    fn commit_message_helpers() {
        assert_eq!(sanitize_commit_message("  Fix it...\nsecond", " body \n"), ("Fix it".into(), "body".into()));
        assert_eq!(sanitize_commit_message(" . ", ""), ("Update project files".into(), String::new()));
        assert_eq!(
            parse_custom_commit_message("Subject\r\n\r\nBody line"),
            Some(("Subject".into(), "Body line".into()))
        );
        assert_eq!(parse_custom_commit_message("   "), None);
        assert_eq!(format_commit_message("S", "  "), "S");
        assert_eq!(format_commit_message("S", " B "), "S\n\nB");
        assert_eq!(truncate_text(Some(&"x".repeat(80)), 72).unwrap().len(), 72);
        assert_eq!(truncate_text(Some(""), 72), None);
        assert_eq!(sanitize_progress_text("  \n "), None);
    }

    #[test]
    fn feature_branch_names_avoid_collisions() {
        let existing = vec!["main".to_owned(), "feature/update".to_owned(), "Feature/Update-2".to_owned()];
        assert_eq!(resolve_auto_feature_branch_name(&existing, None), "feature/update-3");
        assert_eq!(resolve_auto_feature_branch_name(&existing, Some("Add login")), "feature/add-login");
        assert_eq!(resolve_pull_request_worktree_local_branch_name(7, "main", Some(true)), "t3code/pr-7/main");
        assert_eq!(resolve_pull_request_worktree_local_branch_name(7, "main", None), "main");
        assert_eq!(normalize_pull_request_reference(" #42 "), "42");
        assert_eq!(normalize_pull_request_reference("https://x/pull/1"), "https://x/pull/1");
    }
}
