//! Pull request keys: the parts of `packages/shared/src/{changeRequestUrl,sourceControl,
//! threadPullRequests}.ts` that persistence depends on (migration 050 and the
//! `ProjectionThreadPullRequestRepository`, which normalize every key before writing or
//! matching it).

use std::sync::OnceLock;

use regex::Regex;
use url::Url;

/// A change request named the way a thread link names one (`ChangeRequestLink`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequestLink {
    pub host: String,
    pub repository: String,
    pub number: i64,
    /// Forgejo's HTTP host and port, separate from the portless repository identity.
    pub authority: Option<String>,
}

/// `ThreadPullRequestKey`: host-level identity of a linked pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadPullRequestKey {
    pub host: String,
    pub repository: String,
    pub number: i64,
}

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// `String.prototype.trim`: Unicode white space plus the BOM.
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// `canonicalRepositoryKey`: Azure DevOps SSH and legacy `visualstudio.com` keys become
/// `dev.azure.com/<org>/<project>/_git/<repo>`.
pub fn canonical_repository_key(key: &str) -> String {
    static SSH: OnceLock<Regex> = OnceLock::new();
    static LEGACY: OnceLock<Regex> = OnceLock::new();
    let ssh = regex(&SSH, r"^(?:ssh\.dev\.azure\.com|vs-ssh\.visualstudio\.com)/v3/([^/]+)/([^/]+)/([^/]+)$");
    let legacy = regex(&LEGACY, r"^([^.]+)\.visualstudio\.com/(?:defaultcollection/)?([^/]+)/_git/([^/]+)$");
    let first = ssh.replace(key, "dev.azure.com/${1}/${2}/_git/${3}").into_owned();
    legacy.replace(&first, "dev.azure.com/${1}/${2}/_git/${3}").into_owned()
}

/// The host itself, one of its subdomains, or an install named after the provider.
fn is_host_of(hostname: &str, apex: &str, label: Option<&str>) -> bool {
    if hostname == apex || hostname.ends_with(&format!(".{apex}")) {
        return true;
    }
    label.is_some_and(|label| hostname.split('.').any(|part| part == label))
}

fn claim(host: &str, captures: Option<regex::Captures<'_>>) -> Option<ChangeRequestLink> {
    let captures = captures?;
    let repository = captures.get(1)?.as_str();
    // `Number(match[2])` then `Number.isSafeInteger(number) && number > 0`.
    let digits = captures.get(2)?.as_str();
    let number = digits.parse::<u128>().ok().filter(|n| *n <= MAX_SAFE_INTEGER as u128)? as i64;
    if repository.is_empty() || number <= 0 {
        return None;
    }
    Some(ChangeRequestLink {
        host: host.to_string(),
        repository: repository.to_lowercase(),
        number,
        authority: None,
    })
}

/// `URL.host`: hostname plus a non-default port.
fn url_authority(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// `parseChangeRequestUrl`: the repository and number behind a change request URL on a host
/// this can read, or `None` for anything else.
pub fn parse_change_request_url(target_url: &str) -> Option<ChangeRequestLink> {
    static GITHUB: OnceLock<Regex> = OnceLock::new();
    static FORGEJO: OnceLock<Regex> = OnceLock::new();
    static GITLAB: OnceLock<Regex> = OnceLock::new();
    static BITBUCKET: OnceLock<Regex> = OnceLock::new();
    static AZURE: OnceLock<Regex> = OnceLock::new();

    let url = Url::parse(target_url).ok()?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return None;
    }
    let host = url.host_str().unwrap_or("").to_lowercase();
    let path = url.path();

    if is_host_of(&host, "github.com", Some("github")) {
        let github = regex(&GITHUB, r"^/([^/]+/[^/]+)/pull/([0-9]+)(?:/|$)");
        if let Some(captures) = github.captures(path) {
            return claim(&host, Some(captures));
        }
    }
    let forgejo = regex(&FORGEJO, r"^/([^/]+(?:/[^/]+)+)/pulls/([0-9]+)(?:/|$)");
    if let Some(captures) = forgejo.captures(path) {
        return claim(&host, Some(captures)).map(|link| ChangeRequestLink {
            authority: Some(url_authority(&url).to_lowercase()),
            ..link
        });
    }
    let gitlab = regex(&GITLAB, r"^/([^/]+(?:/[^/]+)+)/-/merge_requests/([0-9]+)(?:/|$)");
    if let Some(captures) = gitlab.captures(path) {
        return claim(&host, Some(captures));
    }
    if is_host_of(&host, "bitbucket.org", Some("bitbucket")) {
        let bitbucket = regex(&BITBUCKET, r"^/([^/]+/[^/]+)/pull-requests/([0-9]+)(?:/|$)");
        return claim(&host, bitbucket.captures(path));
    }
    if is_host_of(&host, "dev.azure.com", None) || host.ends_with(".visualstudio.com") {
        let azure = regex(&AZURE, r"^/((?:[^/]+/)*_git/[^/]+)/pullrequest/([0-9]+)(?:/|$)");
        return claim(&host, azure.captures(path));
    }
    None
}

/// `normalizeThreadPullRequestKey`: lower-cased, canonical (Azure) key, recovering a Forgejo
/// HTTP authority from the link's URL when it names the same pull request.
pub fn normalize_thread_pull_request_key(host: &str, repository: &str, number: i64, authority: Option<&str>, url: Option<&str>) -> ThreadPullRequestKey {
    let parsed = url.and_then(parse_change_request_url);
    let authority: Option<String> = match authority {
        Some(authority) => Some(authority.to_string()),
        None => parsed.and_then(|parsed| {
            if parsed.repository == js_trim(repository).to_lowercase() && parsed.number == number {
                parsed.authority
            } else {
                None
            }
        }),
    };
    let base = authority.as_deref().unwrap_or(host);
    let canonical = canonical_repository_key(&format!("{}/{}", js_trim(base).to_lowercase(), js_trim(repository).to_lowercase()));
    match canonical.find('/') {
        Some(separator) => ThreadPullRequestKey {
            host: canonical[..separator].to_string(),
            repository: canonical[separator + 1..].to_string(),
            number,
        },
        // `indexOf` -1: `slice(0, -1)` / `slice(0)`; unreachable since a '/' is always inserted.
        None => ThreadPullRequestKey {
            host: canonical[..canonical.len().saturating_sub(1)].to_string(),
            repository: canonical.clone(),
            number,
        },
    }
}

/// `legacyThreadPullRequestKey`: the key for a pre-050 `linked_pull_request_json` link. Legacy
/// Azure selectors omit the organization and project, so those come from the URL.
pub fn legacy_thread_pull_request_key(repository: &str, number: i64, url: &str, fallback_host: Option<&str>) -> ThreadPullRequestKey {
    if let Some(parsed) = parse_change_request_url(url) {
        if parsed.number == number {
            let canonical = canonical_repository_key(&format!("{}/{}", parsed.host, parsed.repository));
            if parsed.authority.is_some() || canonical.starts_with("dev.azure.com/") {
                return normalize_thread_pull_request_key(&parsed.host, &parsed.repository, parsed.number, parsed.authority.as_deref(), None);
            }
        }
    }
    let host = match fallback_host {
        Some(host) => host.to_string(),
        None => match Url::parse(url) {
            Ok(parsed) => parsed.host_str().unwrap_or("").to_string(),
            Err(_) => "unknown".to_string(),
        },
    };
    let host = js_trim(&host).to_lowercase();
    ThreadPullRequestKey {
        host: if host.is_empty() { "unknown".to_string() } else { host },
        repository: js_trim(repository).to_lowercase(),
        number,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_like_the_shared_package() {
        let github = parse_change_request_url("https://GitHub.com/PingDotGG/T3Code/pull/42/files").expect("github");
        assert_eq!(github.host, "github.com");
        assert_eq!(github.repository, "pingdotgg/t3code");
        assert_eq!(github.number, 42);
        assert_eq!(github.authority, None);

        let forgejo = parse_change_request_url("http://Code.Example.test:3000/team/app/pulls/7").unwrap();
        assert_eq!(forgejo.host, "code.example.test");
        assert_eq!(forgejo.authority.as_deref(), Some("code.example.test:3000"));

        let gitlab = parse_change_request_url("https://git.corp.test/a/b/c/-/merge_requests/3").unwrap();
        assert_eq!(gitlab.repository, "a/b/c");

        let azure = parse_change_request_url("https://dev.azure.com/Org/Proj/_git/Web/pullrequest/9").unwrap();
        assert_eq!(azure.repository, "org/proj/_git/web");

        assert!(parse_change_request_url("https://github.com/a/b/issues/1").is_none());
        assert!(parse_change_request_url("not a url").is_none());
        assert!(parse_change_request_url("mailto:x@y.z").is_none());
        assert!(parse_change_request_url("https://github.com/a/b/pull/0").is_none());
        assert!(parse_change_request_url("https://github.com/a/b/pull/99999999999999999999").is_none());
    }

    #[test]
    fn canonicalizes_azure_keys() {
        assert_eq!(
            canonical_repository_key("ssh.dev.azure.com/v3/org/proj/repo"),
            "dev.azure.com/org/proj/_git/repo"
        );
        assert_eq!(
            canonical_repository_key("org.visualstudio.com/defaultcollection/proj/_git/repo"),
            "dev.azure.com/org/proj/_git/repo"
        );
        assert_eq!(canonical_repository_key("github.com/a/b"), "github.com/a/b");
    }

    #[test]
    fn legacy_keys_follow_the_migration_cases() {
        let github = legacy_thread_pull_request_key("PingDotGG/T3Code", 42, "https://GitHub.com/pingdotgg/t3code/pull/42", None);
        assert_eq!(
            github,
            ThreadPullRequestKey {
                host: "github.com".into(),
                repository: "pingdotgg/t3code".into(),
                number: 42
            }
        );
        let bad = legacy_thread_pull_request_key("acme/widgets", 7, "not a url", None);
        assert_eq!(bad.host, "unknown");
        assert_eq!(bad.repository, "acme/widgets");
        let azure = legacy_thread_pull_request_key("web", 7, "https://dev.azure.com/org-a/project/_git/web/pullrequest/7", None);
        assert_eq!(azure.host, "dev.azure.com");
        assert_eq!(azure.repository, "org-a/project/_git/web");
    }

    #[test]
    fn normalizes_forgejo_authority_from_url() {
        let key = normalize_thread_pull_request_key("code.example.test", "Team/App", 7, None, Some("http://code.example.test:3000/team/app/pulls/7"));
        assert_eq!(key.host, "code.example.test:3000");
        assert_eq!(key.repository, "team/app");
        let other = normalize_thread_pull_request_key("code.example.test", "team/app", 8, None, Some("http://code.example.test:3000/team/app/pulls/7"));
        assert_eq!(other.host, "code.example.test");
    }
}
