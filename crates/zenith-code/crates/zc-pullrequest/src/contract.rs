//! The helper functions of `packages/contracts/src/pullRequest.ts` the server uses:
//! `pullRequestHostOf`, `resolvePullRequestAuthorFilter`, `pullRequestProviderRequirement`.

use std::sync::OnceLock;

use regex::Regex;
use zc_contracts::{PullRequestUnavailableReason, SourceControlProviderKind};

/// `PROVIDER_REQUIREMENT`: `(missing, unauthenticated)` per host kind.
fn provider_requirement(provider: SourceControlProviderKind) -> Option<(&'static str, &'static str)> {
    match provider {
        SourceControlProviderKind::Github => Some((
            "GitHub CLI (`gh`) is required to browse change requests on this host. Install it from https://cli.github.com/ and reload.",
            "GitHub CLI is not authenticated. Run `gh auth login` and retry.",
        )),
        SourceControlProviderKind::Forgejo => Some((
            "Install Forgejo CLI (`fj` 0.6 or later) from https://codeberg.org/forgejo-contrib/forgejo-cli or Gitea CLI (`tea` 0.16 or later) from https://gitea.com/gitea/tea to browse Forgejo pull requests.",
            "Authenticate your Forgejo or Gitea server with `fj --host <server-url> auth add-token` on the T3 Code server. If fj is missing or unconfigured for that server, use `tea login add`. A configured fj account must be repaired with fj.",
        )),
        SourceControlProviderKind::Gitlab => Some((
            "GitLab CLI (`glab`) is required to browse change requests on this host. Install it from https://gitlab.com/gitlab-org/cli and reload.",
            "GitLab CLI is not authenticated. Run `glab auth login` and retry.",
        )),
        SourceControlProviderKind::AzureDevops => Some((
            "Azure CLI (`az`) with the Azure DevOps extension is required. Install `az`, then run `az extension add --name azure-devops`.",
            "Azure CLI is not signed in. Run `az login` and retry.",
        )),
        SourceControlProviderKind::Bitbucket => Some((
            "Bitbucket needs API credentials on the server. Add them in Settings → Source Control.",
            "Bitbucket rejected the configured credentials. Check them in Settings → Source Control.",
        )),
        SourceControlProviderKind::Unknown => None,
    }
}

/// `pullRequestProviderRequirement(provider, reason)`.
pub fn pull_request_provider_requirement(provider: SourceControlProviderKind, reason: PullRequestUnavailableReason) -> Option<&'static str> {
    let (missing, unauthenticated) = provider_requirement(provider)?;
    match reason {
        PullRequestUnavailableReason::CliMissing => Some(missing),
        PullRequestUnavailableReason::CliUnauthenticated => Some(unauthenticated),
        PullRequestUnavailableReason::ProviderUnsupported => None,
    }
}

/// The `PullRequestUnavailableError` `message` getter.
pub fn unavailable_message(reason: PullRequestUnavailableReason, provider: Option<SourceControlProviderKind>) -> String {
    let requirement = provider.and_then(provider_requirement);
    match reason {
        PullRequestUnavailableReason::CliMissing => requirement
            .map(|(missing, _)| missing)
            .unwrap_or("The tool this host is read through is not installed or set up.")
            .to_owned(),
        PullRequestUnavailableReason::CliUnauthenticated => requirement
            .map(|(_, unauthenticated)| unauthenticated)
            .unwrap_or("This host has no working credentials.")
            .to_owned(),
        PullRequestUnavailableReason::ProviderUnsupported => "Change requests cannot be browsed for this project's host yet.".to_owned(),
    }
}

/// `pullRequestHostOf(identity, kind)`: the host a project's repository is addressed below.
/// `canonical_key` and `remote_url` are the repository identity's fields, when it has them.
pub fn pull_request_host_of(canonical_key: Option<&str>, remote_url: Option<&str>, kind: SourceControlProviderKind) -> String {
    if kind == SourceControlProviderKind::Forgejo {
        if let Some(remote) = remote_url.and_then(|remote| url::Url::parse(remote).ok()) {
            if remote.scheme() == "http" || remote.scheme() == "https" {
                return zc_sourcecontrol::util::url_host(&remote).to_lowercase();
            }
        }
    }
    let host = canonical_key.and_then(|key| key.split('/').next()).map(zc_sourcecontrol::util::js_trim);
    match host {
        Some(host) if !host.is_empty() => host.to_lowercase(),
        _ => kind.as_str().to_owned(),
    }
}

/// `resolvePullRequestAuthorFilter(author, viewer)`: `author:me` names whoever is signed in.
pub fn resolve_pull_request_author_filter(author: &str, viewer: Option<&str>) -> String {
    static ME: OnceLock<Regex> = OnceLock::new();
    let trimmed = zc_sourcecontrol::util::js_trim(author);
    if !ME.get_or_init(|| Regex::new(r"(?i)^@?me$").expect("valid regex")).is_match(trimmed) {
        return trimmed.to_owned();
    }
    match viewer {
        Some(viewer) if !zc_sourcecontrol::util::js_trim(viewer).is_empty() => viewer.to_owned(),
        _ => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_reads_the_canonical_key_or_falls_back_to_the_kind() {
        assert_eq!(
            pull_request_host_of(Some("GitHub.com/acme/widgets"), None, SourceControlProviderKind::Github),
            "github.com"
        );
        assert_eq!(pull_request_host_of(None, None, SourceControlProviderKind::Gitlab), "gitlab");
        assert_eq!(
            pull_request_host_of(
                Some("forge.example.test/a/b"),
                Some("https://Forge.Example.test:3000/a/b.git"),
                SourceControlProviderKind::Forgejo
            ),
            "forge.example.test:3000"
        );
        assert_eq!(
            pull_request_host_of(
                Some("forge.example.test/a/b"),
                Some("git@forge.example.test:a/b.git"),
                SourceControlProviderKind::Forgejo
            ),
            "forge.example.test"
        );
    }

    #[test]
    fn author_me_resolves_to_the_viewer() {
        assert_eq!(resolve_pull_request_author_filter(" @Me ", Some("octo")), "octo");
        assert_eq!(resolve_pull_request_author_filter("me", None), "me");
        assert_eq!(resolve_pull_request_author_filter("me", Some("  ")), "me");
        assert_eq!(resolve_pull_request_author_filter(" someone ", Some("octo")), "someone");
    }
}
