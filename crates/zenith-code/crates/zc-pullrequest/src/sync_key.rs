//! `pullRequest/pullRequestSyncKey.ts`: convert checkout-scoped pull request references to the
//! host-level identity linked threads use, plus `resolvePullRequestSyncKey` of `ws.ts` (the
//! `pullRequests.linkedThreads`, `runAction` and `invalidate` handlers resolve the key with it).
//!
//! Also the two `RepositoryIdentity` helpers they need: `sourceControlRepositorySelector`
//! (`shared/sourceControl.ts`) and `pullRequestHostOf(identity, identity.provider)`
//! (`contracts/pullRequest.ts`).

use zc_contracts::{OrchestrationProjectShell, PullRequestRef, RepositoryIdentity, ThreadPullRequestKey};
use zc_db::pr_keys::normalize_thread_pull_request_key;
use zc_ports::ProjectionReads;

/// `sourceControlRepositorySelector(identity)`: the repository name a provider CLI accepts.
pub fn source_control_repository_selector(identity: Option<&RepositoryIdentity>) -> Option<String> {
    let identity = identity?;
    let non_empty = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    if identity.provider.as_deref() == Some("azure-devops") {
        let display_name = identity.display_name.clone().unwrap_or_default();
        let last = display_name.split('/').rfind(|part| *part != "_git").map(str::to_owned);
        return non_empty(&identity.name).or(last.filter(|part| !part.is_empty()));
    }
    if let Some(display_name) = non_empty(&identity.display_name) {
        return Some(display_name);
    }
    match (non_empty(&identity.owner), non_empty(&identity.name)) {
        (Some(owner), Some(name)) => Some(format!("{owner}/{name}")),
        _ => None,
    }
}

/// `pullRequestHostOf(identity, kind)` with a free-form `kind` (`identity.provider`, which can
/// be absent): `None` where TS returns `undefined`.
pub fn identity_host_of(identity: &RepositoryIdentity, kind: Option<&str>) -> Option<String> {
    if kind == Some("forgejo") {
        if let Ok(remote) = url::Url::parse(&identity.locator.remote_url) {
            if remote.scheme() == "http" || remote.scheme() == "https" {
                return Some(zc_sourcecontrol::util::url_host(&remote).to_lowercase());
            }
        }
    }
    let host = identity.canonical_key.split('/').next().map(zc_sourcecontrol::util::js_trim).unwrap_or("");
    if host.is_empty() {
        kind.map(str::to_owned)
    } else {
        Some(host.to_lowercase())
    }
}

fn normalized(host: &str, repository: &str, number: i64) -> ThreadPullRequestKey {
    let key = normalize_thread_pull_request_key(host, repository, number, None, None);
    ThreadPullRequestKey {
        host: key.host,
        repository: key.repository,
        number: key.number,
    }
}

/// `pullRequestSyncKey(reference, identity?)`: convert checkout-scoped references to the
/// host-level identity used by linked threads. `None` when the host cannot be known or a hostless
/// Azure reference names another repository than the checkout's.
pub fn pull_request_sync_key(reference: &PullRequestRef, identity: Option<&RepositoryIdentity>) -> Option<ThreadPullRequestKey> {
    if let Some(identity) = identity.filter(|identity| identity.provider.as_deref() == Some("azure-devops")) {
        if !reference.repository.contains('/') {
            let selector = source_control_repository_selector(Some(identity)).map(|selector| selector.to_lowercase());
            let host_mismatch = reference
                .host
                .as_ref()
                .is_some_and(|host| Some(host.to_lowercase()) != identity_host_of(identity, Some("azure-devops")));
            if Some(reference.repository.to_lowercase()) != selector || host_mismatch {
                return None;
            }
            let mut parts = identity.canonical_key.split('/');
            let host = parts.next().unwrap_or("");
            let repository: Vec<&str> = parts.collect();
            if host.is_empty() || repository.is_empty() {
                return None;
            }
            return Some(normalized(host, &repository.join("/"), reference.number));
        }
    }
    let host = match &reference.host {
        Some(host) => Some(host.clone()),
        None => identity.and_then(|identity| identity_host_of(identity, identity.provider.as_deref())),
    };
    host.map(|host| normalized(&host, &reference.repository, reference.number))
}

/// `resolvePullRequestSyncKey(reference)` (`ws.ts`): a reference's host-level link key; the
/// project's own host where the ref names none. `None` when it cannot be resolved, including
/// when the project read fails.
pub async fn resolve_pull_request_sync_key(projections: &dyn ProjectionReads, reference: &PullRequestRef) -> Option<ThreadPullRequestKey> {
    if reference.host.is_some() && reference.repository.contains('/') {
        return pull_request_sync_key(reference, None);
    }
    let project: Option<OrchestrationProjectShell> = projections.get_project_shell_by_id(&reference.project_id).await.ok()?;
    let identity = project.and_then(|project| project.repository_identity.flatten());
    pull_request_sync_key(reference, identity.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reference(value: serde_json::Value) -> PullRequestRef {
        let mut base = json!({"projectId": "project", "repository": "web", "number": 7});
        for (key, value) in value.as_object().unwrap() {
            base[key] = value.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    fn key(host: &str, repository: &str, number: i64) -> Option<ThreadPullRequestKey> {
        Some(ThreadPullRequestKey {
            host: host.into(),
            repository: repository.into(),
            number,
        })
    }

    #[test]
    fn resolves_hostless_and_checkout_host_azure_references() {
        for canonical_key in [
            "dev.azure.com/org/project/_git/web",
            "ssh.dev.azure.com/v3/org/project/web",
            "vs-ssh.visualstudio.com/v3/org/project/web",
            "org.visualstudio.com/defaultcollection/project/_git/web",
        ] {
            let display_name = canonical_key.split('/').skip(1).collect::<Vec<_>>().join("/");
            let identity: RepositoryIdentity = serde_json::from_value(json!({
                "canonicalKey": canonical_key,
                "provider": "azure-devops",
                "name": "web",
                "displayName": display_name,
                "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": format!("https://{canonical_key}")},
            }))
            .unwrap();
            let expected = key("dev.azure.com", "org/project/_git/web", 7);
            let checkout_host = canonical_key.split('/').next().unwrap();
            assert_eq!(pull_request_sync_key(&reference(json!({})), Some(&identity)), expected, "{canonical_key}");
            assert_eq!(
                pull_request_sync_key(&reference(json!({"host": checkout_host})), Some(&identity)),
                expected,
                "{canonical_key}"
            );
            assert_eq!(pull_request_sync_key(&reference(json!({"repository": "other"})), Some(&identity)), None);
            assert_eq!(pull_request_sync_key(&reference(json!({"host": "unrelated.test"})), Some(&identity)), None);
        }
    }

    #[test]
    fn normalizes_complete_hosted_aliases_without_requiring_a_checkout() {
        assert_eq!(
            pull_request_sync_key(&reference(json!({"host": "org.visualstudio.com", "repository": "project/_git/web"})), None),
            key("dev.azure.com", "org/project/_git/web", 7)
        );
        assert_eq!(
            pull_request_sync_key(&reference(json!({"host": "github.com", "repository": "acme/web"})), None),
            key("github.com", "acme/web", 7)
        );
        assert_eq!(pull_request_sync_key(&reference(json!({})), None), None);
    }

    #[test]
    fn repository_selector_follows_the_provider() {
        let identity = |value: serde_json::Value| -> RepositoryIdentity {
            let mut base = json!({"canonicalKey": "github.com/acme/web", "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "git@github.com:acme/web.git"}});
            for (key, value) in value.as_object().unwrap() {
                base[key] = value.clone();
            }
            serde_json::from_value(base).unwrap()
        };
        assert_eq!(source_control_repository_selector(None), None);
        assert_eq!(
            source_control_repository_selector(Some(&identity(json!({"displayName": "acme/web"})))),
            Some("acme/web".into())
        );
        assert_eq!(
            source_control_repository_selector(Some(&identity(json!({"owner": "acme", "name": "web"})))),
            Some("acme/web".into())
        );
        assert_eq!(source_control_repository_selector(Some(&identity(json!({"name": "web"})))), None);
        assert_eq!(
            source_control_repository_selector(Some(&identity(json!({"provider": "azure-devops", "displayName": "org/project/_git/web"})))),
            Some("web".into())
        );
        assert_eq!(
            identity_host_of(
                &identity(json!({"locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "http://Forge.test:3000/a/b"}})),
                Some("forgejo")
            ),
            Some("forge.test:3000".into())
        );
        assert_eq!(identity_host_of(&identity(json!({"canonicalKey": " "})), None), None);
    }
}
