//! The parts of `packages/shared/src/git.ts` and `sourceControl.ts` the VCS layer uses:
//! remote URL normalization, remote-ref deduplication, forge detection from a remote URL.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::contracts::{SourceControlProviderInfo, SourceControlProviderKind, VcsRef};

/// `deriveLocalBranchNameFromRemoteRef` (shared): everything after the first `/`, or the name
/// itself.
pub fn derive_local_branch_name_from_remote_ref(branch_name: &str) -> String {
    match branch_name.find('/') {
        Some(index) if index > 0 && index != branch_name.len() - 1 => branch_name[index + 1..].to_owned(),
        _ => branch_name.to_owned(),
    }
}

fn derive_local_branch_name_candidates(branch_name: &str, remote_name: Option<&str>) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    let first = derive_local_branch_name_from_remote_ref(branch_name);
    if !first.is_empty() {
        candidates.push(first);
    }
    if let Some(remote_name) = remote_name.filter(|name| !name.is_empty()) {
        let prefix = format!("{remote_name}/");
        if branch_name.starts_with(&prefix) && branch_name.len() > prefix.len() {
            let candidate = branch_name[prefix.len()..].to_owned();
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

/// `dedupeRemoteBranchesWithLocalMatches`: hide `origin/*` refs whose local branch exists.
pub fn dedupe_remote_branches_with_local_matches(refs: Vec<VcsRef>) -> Vec<VcsRef> {
    let local_names: HashSet<String> = refs.iter().filter(|r| !r.is_remote()).map(|r| r.name.clone()).collect();
    refs.into_iter()
        .filter(|r| {
            if !r.is_remote() {
                return true;
            }
            if r.remote_name.as_deref() != Some("origin") {
                return true;
            }
            !derive_local_branch_name_candidates(&r.name, r.remote_name.as_deref())
                .iter()
                .any(|candidate| local_names.contains(candidate))
        })
        .collect()
}

fn azure_dev_ops_repository_key(host: &str, segments: &[&str]) -> Option<String> {
    if host != "ssh.dev.azure.com" && host != "vs-ssh.visualstudio.com" {
        return None;
    }
    if segments.len() != 4 || segments[0] != "v3" {
        return None;
    }
    let (organization, project, repository) = (segments[1], segments[2], segments[3]);
    if organization.is_empty() || project.is_empty() || repository.is_empty() {
        return None;
    }
    Some(if host == "ssh.dev.azure.com" {
        format!("dev.azure.com/{organization}/{project}/_git/{repository}")
    } else {
        format!("{organization}.visualstudio.com/{project}/_git/{repository}")
    })
}

/// `normalizeGitRemoteUrl`: a stable comparison key for a remote URL.
pub fn normalize_git_remote_url(value: &str) -> String {
    static TRAILING_SLASHES: OnceLock<Regex> = OnceLock::new();
    static DOT_GIT: OnceLock<Regex> = OnceLock::new();
    static SCHEME: OnceLock<Regex> = OnceLock::new();
    static SCP: OnceLock<Regex> = OnceLock::new();
    let trailing = TRAILING_SLASHES.get_or_init(|| Regex::new(r"/+$").unwrap());
    let dot_git = DOT_GIT.get_or_init(|| Regex::new(r"(?i)\.git$").unwrap());
    let scheme = SCHEME.get_or_init(|| Regex::new(r"(?i)^(?:ssh|https?|git)://").unwrap());
    let scp = SCP.get_or_init(|| Regex::new(r"(?i)^[a-zA-Z0-9._-]+@([^:/\s]+):([^/\s]+(?:/[^/\s]+)+)$").unwrap());

    let without_slashes = trailing.replace(value.trim(), "");
    let normalized = dot_git.replace(&without_slashes, "").to_lowercase();

    if scheme.is_match(&normalized) {
        match url::Url::parse(&normalized) {
            Ok(url) => {
                let segments: Vec<&str> = url.path().split('/').filter(|segment| !segment.is_empty()).collect();
                if let Some(host) = url.host_str().filter(|host| !host.is_empty()) {
                    if segments.len() > 1 {
                        return azure_dev_ops_repository_key(host, &segments).unwrap_or_else(|| format!("{host}/{}", segments.join("/")));
                    }
                }
            }
            Err(_) => return normalized,
        }
    }

    if let Some(captures) = scp.captures(&normalized) {
        let host = captures.get(1).map(|m| m.as_str()).unwrap_or_default();
        let path = captures.get(2).map(|m| m.as_str()).unwrap_or_default();
        if !host.is_empty() && !path.is_empty() {
            let segments: Vec<&str> = path.split('/').collect();
            return azure_dev_ops_repository_key(host, &segments).unwrap_or_else(|| format!("{host}/{path}"));
        }
    }

    normalized
}

fn scp_ssh_host(remote_url: &str) -> Option<String> {
    static SCP: OnceLock<Regex> = OnceLock::new();
    let scp = SCP.get_or_init(|| Regex::new(r"^[a-zA-Z0-9._-]+@([^:/]+):").unwrap());
    scp.captures(remote_url).and_then(|captures| captures.get(1)).map(|m| m.as_str().to_owned())
}

/// `isSshRemoteUrl`.
pub fn is_ssh_remote_url(remote_url: &str) -> bool {
    let trimmed = remote_url.trim();
    scp_ssh_host(trimmed).is_some() || trimmed.to_lowercase().starts_with("ssh://")
}

/// WHATWG `url.host` (hostname plus a non-default port).
fn url_host(url: &url::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

fn parse_remote_host(remote_url: &str) -> Option<String> {
    let trimmed = remote_url.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(host) = scp_ssh_host(trimmed) {
        return Some(host.to_lowercase());
    }
    let url = url::Url::parse(trimmed).ok()?;
    let host = if url.scheme() == "ssh" {
        url.host_str().map(str::to_owned)
    } else {
        url_host(&url)
    }?;
    Some(host.to_lowercase())
}

fn parse_host_name(host: &str) -> String {
    match url::Url::parse(&format!("https://{host}")) {
        Ok(url) => url.host_str().unwrap_or_default().to_lowercase(),
        Err(_) => {
            static PORT: OnceLock<Regex> = OnceLock::new();
            PORT.get_or_init(|| Regex::new(r":\d+$").unwrap()).replace(host, "").to_lowercase()
        }
    }
}

fn has_dns_label(host: &str, label: &str) -> bool {
    host.split('.').any(|part| part == label)
}

/// `detectSourceControlProviderFromRemoteUrl` (a.k.a. `detectSourceControlProviderFromGitRemoteUrl`).
pub fn detect_source_control_provider_from_remote_url(remote_url: &str) -> Option<SourceControlProviderInfo> {
    let host = parse_remote_host(remote_url)?;
    let hostname = parse_host_name(&host);
    let base_url = format!("https://{host}");
    let info = |kind, name: &str, base_url: String| SourceControlProviderInfo {
        kind,
        name: name.to_owned(),
        base_url,
    };

    if hostname == "codeberg.org" || has_dns_label(&hostname, "forgejo") || has_dns_label(&hostname, "gitea") {
        let trimmed = remote_url.trim();
        let is_web = trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case("http:") || trimmed.len() >= 6 && trimmed[..6].eq_ignore_ascii_case("https:");
        let base = if is_web {
            url::Url::parse(trimmed).map(|url| url.origin().ascii_serialization()).unwrap_or(base_url)
        } else {
            base_url
        };
        return Some(info(SourceControlProviderKind::Forgejo, "Forgejo", base));
    }
    if hostname == "github.com" || has_dns_label(&hostname, "github") {
        let name = if hostname == "github.com" { "GitHub" } else { "GitHub Self-Hosted" };
        return Some(info(SourceControlProviderKind::Github, name, base_url));
    }
    if hostname == "gitlab.com" || has_dns_label(&hostname, "gitlab") {
        let name = if hostname == "gitlab.com" { "GitLab" } else { "GitLab Self-Hosted" };
        return Some(info(SourceControlProviderKind::Gitlab, name, base_url));
    }
    if hostname == "dev.azure.com" || hostname.ends_with(".dev.azure.com") || hostname.ends_with(".visualstudio.com") {
        return Some(info(SourceControlProviderKind::AzureDevops, "Azure DevOps", base_url));
    }
    if hostname == "bitbucket.org" || has_dns_label(&hostname, "bitbucket") {
        let name = if hostname == "bitbucket.org" { "Bitbucket" } else { "Bitbucket Self-Hosted" };
        return Some(info(SourceControlProviderKind::Bitbucket, name, base_url));
    }
    Some(info(SourceControlProviderKind::Unknown, &host, base_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(name: &str, remote: Option<&str>) -> VcsRef {
        VcsRef {
            name: name.into(),
            is_remote: Some(remote.is_some()),
            remote_name: remote.map(Into::into),
            current: false,
            is_default: false,
            worktree_path: None,
        }
    }

    #[test]
    fn dedupes_origin_refs_with_local_matches() {
        let refs = vec![
            r("main", None),
            r("origin/main", Some("origin")),
            r("origin/feature", Some("origin")),
            r("fork/main", Some("fork")),
        ];
        let names: Vec<String> = dedupe_remote_branches_with_local_matches(refs).into_iter().map(|r| r.name).collect();
        assert_eq!(names, vec!["main", "origin/feature", "fork/main"]);
    }

    #[test]
    fn normalizes_remote_urls_across_transports() {
        let https = normalize_git_remote_url("https://github.com/Owner/Repo.git/");
        let ssh = normalize_git_remote_url("git@github.com:owner/repo.git");
        let ssh_url = normalize_git_remote_url("ssh://git@github.com/owner/repo");
        assert_eq!(https, "github.com/owner/repo");
        assert_eq!(ssh, https);
        assert_eq!(ssh_url, https);
        assert_eq!(
            normalize_git_remote_url("git@ssh.dev.azure.com:v3/org/proj/repo"),
            "dev.azure.com/org/proj/_git/repo"
        );
        assert_eq!(normalize_git_remote_url("/local/path.git"), "/local/path");
    }

    #[test]
    fn detects_forges() {
        let gh = detect_source_control_provider_from_remote_url("git@github.com:o/r.git").unwrap();
        assert_eq!(gh.kind, SourceControlProviderKind::Github);
        assert_eq!(gh.name, "GitHub");
        assert_eq!(gh.base_url, "https://github.com");
        let ghe = detect_source_control_provider_from_remote_url("https://github.acme.io:8443/o/r").unwrap();
        assert_eq!(ghe.name, "GitHub Self-Hosted");
        assert_eq!(ghe.base_url, "https://github.acme.io:8443");
        let forgejo = detect_source_control_provider_from_remote_url("http://codeberg.org/o/r").unwrap();
        assert_eq!(forgejo.base_url, "http://codeberg.org");
        let azure = detect_source_control_provider_from_remote_url("git@ssh.dev.azure.com:v3/o/p/r").unwrap();
        assert_eq!(azure.kind, SourceControlProviderKind::AzureDevops);
        let unknown = detect_source_control_provider_from_remote_url("https://git.example.com/o/r").unwrap();
        assert_eq!(unknown.kind, SourceControlProviderKind::Unknown);
        assert_eq!(unknown.name, "git.example.com");
        assert!(detect_source_control_provider_from_remote_url("/local/path").is_none());
        assert!(is_ssh_remote_url("git@github.com:o/r"));
    }
}
