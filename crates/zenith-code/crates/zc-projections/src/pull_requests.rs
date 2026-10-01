//! The parts of `packages/shared/src/threadPullRequests.ts` (and `sourceControl.ts`,
//! contracts `pullRequestHostOf`) the projections need: link key equality, and the legacy
//! single `linkedPullRequest` derived from a thread's links for clients that predate
//! `pullRequests`. Links are handled in their encoded form (`ThreadPullRequestLink`).

use serde_json::{json, Value};
use url::Url;
use zc_db::pr_keys::{canonical_repository_key, normalize_thread_pull_request_key, parse_change_request_url};

pub use zc_db::pr_keys::legacy_thread_pull_request_key;

/// A key source: `(host, repository, number, url)` (`ThreadPullRequestKey & {url?}`).
pub type KeySource<'a> = (&'a str, &'a str, i64, Option<&'a str>);

/// `threadPullRequestKeyOf`.
pub fn thread_pull_request_key_of(key: KeySource<'_>) -> String {
    let (host, repository, number, url) = key;
    let normalized = normalize_thread_pull_request_key(host, repository, number, None, url);
    format!("{}/{}#{}", normalized.host, normalized.repository, normalized.number)
}

/// `threadPullRequestKeysEqual`.
pub fn thread_pull_request_keys_equal(left: KeySource<'_>, right: KeySource<'_>) -> bool {
    thread_pull_request_key_of(left) == thread_pull_request_key_of(right)
}

fn s<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn link_key_source(link: &Value) -> KeySource<'_> {
    (
        s(link, "host").unwrap_or(""),
        s(link, "repository").unwrap_or(""),
        link.get("number").and_then(Value::as_i64).unwrap_or(0),
        s(link, "url"),
    )
}

fn link_key(link: &Value) -> String {
    thread_pull_request_key_of(link_key_source(link))
}

fn snapshot(link: &Value) -> Option<&Value> {
    link.get("snapshot").filter(|value| !value.is_null())
}

fn is_visible(link: &Value) -> bool {
    s(link, "source") != Some("stack-dismissed")
}

/// Unsynced links count as open: they were just linked.
fn is_open(link: &Value) -> bool {
    snapshot(link).is_none_or(|snapshot| s(snapshot, "state") == Some("open"))
}

/// `Date.parse`, `NaN` as `None`.
fn date_parse(value: Option<&str>) -> Option<f64> {
    value.and_then(zc_core::time::parse_iso_millis).map(|millis| millis as f64)
}

fn latest_updated_at(link: &Value) -> f64 {
    let value = snapshot(link).and_then(|snapshot| s(snapshot, "updatedAt")).or_else(|| s(link, "linkedAt"));
    date_parse(value).unwrap_or(0.0)
}

/// A chain of links, bottom to top (indices into the visible links).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub native: bool,
    pub layers: Vec<usize>,
}

/// `resolveThreadPullRequestChains` over already visible links.
pub fn resolve_chains(visible: &[&Value]) -> Vec<Chain> {
    let mut chains: Vec<Chain> = Vec::new();
    let mut placed: Vec<String> = Vec::new();

    let mut native: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, link) in visible.iter().enumerate() {
        let Some(stack) = link.get("stack").filter(|value| !value.is_null()) else {
            continue;
        };
        let (host, repository, number, url) = link_key_source(link);
        let key = normalize_thread_pull_request_key(host, repository, number, None, url);
        let stack_key = format!("{}/{}#stack:{}", key.host, key.repository, js_string(stack.get("id")));
        match native.iter_mut().find(|(existing, _)| *existing == stack_key) {
            Some(entry) => entry.1.push(index),
            None => native.push((stack_key, vec![index])),
        }
    }
    for (_, mut members) in native {
        let first_layers: Vec<i64> = visible[members[0]]
            .get("stack")
            .and_then(|stack| stack.get("layers"))
            .and_then(Value::as_array)
            .map(|layers| layers.iter().map(|layer| layer.get("number").and_then(Value::as_i64).unwrap_or(0)).collect())
            .unwrap_or_default();
        // `new Map(layers.map((layer, index) => [layer.number, index]))`: later duplicates win.
        let order = |number: i64| -> i64 {
            first_layers
                .iter()
                .rposition(|candidate| *candidate == number)
                .map(|index| index as i64)
                .unwrap_or(0)
        };
        let number_of = |index: usize| visible[index].get("number").and_then(Value::as_i64).unwrap_or(0);
        members.sort_by_key(|index| order(number_of(*index)));
        for member in &members {
            placed.push(link_key(visible[*member]));
        }
        chains.push(Chain { native: true, layers: members });
    }

    let remaining: Vec<usize> = (0..visible.len()).filter(|index| !placed.contains(&link_key(visible[*index]))).collect();
    let branch_key = |index: usize, branch: &str| -> String {
        let (host, repository, number, url) = link_key_source(visible[index]);
        let key = normalize_thread_pull_request_key(host, repository, number, None, url);
        format!("{}/{}:{}", key.host, key.repository, branch)
    };
    let branch_of = |index: usize, field: &str| -> Option<String> { snapshot(visible[index]).map(|snapshot| s(snapshot, field).unwrap_or("").to_string()) };
    // Reused head names cannot identify a parent unambiguously.
    let mut by_head: Vec<(String, Option<usize>)> = Vec::new();
    for &index in &remaining {
        let Some(head) = branch_of(index, "headBranch") else {
            continue;
        };
        let key = branch_key(index, &head);
        match by_head.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = None,
            None => by_head.push((key, Some(index))),
        }
    }
    let lookup = |key: &str| -> Option<usize> { by_head.iter().find(|(existing, _)| existing == key).and_then(|(_, index)| *index) };
    let mut has_child: Vec<String> = Vec::new();
    for &index in &remaining {
        let Some(base) = branch_of(index, "baseBranch") else {
            continue;
        };
        if let Some(parent) = lookup(&branch_key(index, &base)) {
            if parent != index {
                has_child.push(link_key(visible[parent]));
            }
        }
    }
    // Walk from each top (a link nothing builds on) down its base chain.
    for &top in &remaining {
        if has_child.contains(&link_key(visible[top])) {
            continue;
        }
        let mut layers: Vec<usize> = Vec::new();
        let mut cursor = Some(top);
        while let Some(index) = cursor {
            let key = link_key(visible[index]);
            if placed.contains(&key) {
                break;
            }
            placed.push(key);
            layers.insert(0, index);
            cursor = branch_of(index, "baseBranch").and_then(|base| lookup(&branch_key(index, &base)));
        }
        if !layers.is_empty() {
            chains.push(Chain { native: false, layers });
        }
    }
    // Cycles have no top. Keep those links visible without inventing a stack order.
    for &index in &remaining {
        if !placed.contains(&link_key(visible[index])) {
            chains.push(Chain {
                native: false,
                layers: vec![index],
            });
        }
    }
    chains
}

/// `String(value)` for a stack id.
fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => "null".to_string(),
        Some(other) => other.to_string(),
    }
}

/// `resolveThreadCurrentPullRequestLink`: the one link a one-slot surface shows.
pub fn resolve_current_link<'a>(links: &[&'a Value]) -> Option<&'a Value> {
    let visible: Vec<&Value> = links.iter().copied().filter(|link| is_visible(link)).collect();
    if visible.is_empty() {
        return None;
    }
    let open: Vec<usize> = (0..visible.len()).filter(|i| is_open(visible[*i])).collect();
    if open.len() == 1 {
        return Some(visible[open[0]]);
    }
    let chains = resolve_chains(&visible);
    if open.len() > 1 {
        let mut open_chains: Vec<Vec<usize>> = chains
            .iter()
            .map(|chain| chain.layers.iter().rev().copied().filter(|index| is_open(visible[*index])).collect::<Vec<_>>())
            .filter(|layers| !layers.is_empty())
            .collect();
        let newest = |layers: &Vec<usize>| -> f64 {
            layers
                .iter()
                .map(|index| date_parse(s(visible[*index], "linkedAt")).unwrap_or(f64::NAN))
                .fold(
                    f64::NEG_INFINITY,
                    |acc, value| {
                        if acc.is_nan() || value.is_nan() {
                            f64::NAN
                        } else {
                            acc.max(value)
                        }
                    },
                )
        };
        open_chains.sort_by(|left, right| (newest(right) - newest(left)).partial_cmp(&0.0).unwrap_or(std::cmp::Ordering::Equal));
        return open_chains.first().and_then(|layers| layers.first()).map(|index| visible[*index]);
    }
    if chains.len() == 1 {
        return chains[0].layers.last().map(|index| visible[*index]);
    }
    let mut terminal: Vec<&Value> = visible.clone();
    terminal.sort_by(|left, right| {
        (latest_updated_at(right) - latest_updated_at(left))
            .partial_cmp(&0.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    terminal.first().copied()
}

/// `pullRequestHostOf(identity, identity.provider)`; `None` where TS returns `undefined`.
fn pull_request_host_of(identity: &Value) -> Option<String> {
    let kind = s(identity, "provider");
    if kind == Some("forgejo") {
        if let Some(remote) = identity
            .get("locator")
            .and_then(|locator| s(locator, "remoteUrl"))
            .and_then(|url| Url::parse(url).ok())
        {
            if remote.scheme() == "http" || remote.scheme() == "https" {
                return Some(url_host(&remote).to_lowercase());
            }
        }
    }
    let host = s(identity, "canonicalKey").and_then(|key| key.split('/').next()).map(crate::js::trim);
    match host {
        Some(host) if !host.is_empty() => Some(host.to_lowercase()),
        _ => kind.map(str::to_owned),
    }
}

/// WHATWG `URL.host`: host plus a non-default port.
fn url_host(url: &Url) -> String {
    let host = url.host_str().unwrap_or("");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// `sourceControlRepositorySelector`.
fn source_control_repository_selector(identity: &Value) -> Option<String> {
    let display_name = s(identity, "displayName").filter(|name| !name.is_empty());
    let name = s(identity, "name").filter(|name| !name.is_empty());
    if s(identity, "provider") == Some("azure-devops") {
        if let Some(name) = name {
            return Some(name.to_string());
        }
        return display_name
            .unwrap_or("")
            .split('/')
            .rfind(|part| *part != "_git")
            .filter(|part| !part.is_empty())
            .map(str::to_owned);
    }
    if let Some(display_name) = display_name {
        return Some(display_name.to_string());
    }
    match (s(identity, "owner").filter(|owner| !owner.is_empty()), name) {
        (Some(owner), Some(name)) => Some(format!("{owner}/{name}")),
        _ => None,
    }
}

/// `legacyLinkedPullRequestOf`: the encoded `ThreadLinkedPullRequest`, or `None`.
pub fn legacy_linked_pull_request_of(links: &[Value], project_id: &str, identity: Option<&Value>) -> Option<Value> {
    let identity = identity.filter(|identity| !identity.is_null())?;
    let host = pull_request_host_of(identity)?;
    let repository = source_control_repository_selector(identity)?;
    let repository_lower = repository.to_lowercase();
    let azure_key =
        (s(identity, "provider") == Some("azure-devops")).then(|| canonical_repository_key(&s(identity, "canonicalKey").unwrap_or("").to_lowercase()));
    let remote = identity
        .get("locator")
        .and_then(|locator| s(locator, "remoteUrl"))
        .and_then(|url| Url::parse(url).ok());
    let candidates: Vec<&Value> = links
        .iter()
        .filter(|link| {
            if let Some(azure_key) = &azure_key {
                let key = legacy_thread_pull_request_key(
                    s(link, "repository").unwrap_or(""),
                    link.get("number").and_then(Value::as_i64).unwrap_or(0),
                    s(link, "url").unwrap_or(""),
                    s(link, "host"),
                );
                return canonical_repository_key(&format!("{}/{}", key.host, key.repository)) == *azure_key;
            }
            let parsed = parse_change_request_url(s(link, "url").unwrap_or(""));
            if let Some(parsed) = parsed.filter(|parsed| parsed.authority.is_some()) {
                if let Some(remote) = remote.as_ref().filter(|remote| remote.scheme() == "http" || remote.scheme() == "https") {
                    return parsed.authority.as_deref() == Some(url_host(remote).as_str()) && parsed.repository == repository_lower;
                }
                return parsed.host == host && parsed.repository == repository_lower;
            }
            s(link, "host").unwrap_or("").to_lowercase() == host.to_lowercase() && s(link, "repository").unwrap_or("").to_lowercase() == repository_lower
        })
        .collect();
    let link = resolve_current_link(&candidates)?;
    Some(json!({
        "projectId": project_id,
        "repository": if azure_key.is_none() { link.get("repository").cloned().unwrap_or(Value::Null) } else { Value::String(repository) },
        "number": link.get("number").cloned().unwrap_or(Value::Null),
        "url": link.get("url").cloned().unwrap_or(Value::Null),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(number: i64, state: Option<&str>, head: &str, base: &str, linked_at: &str) -> Value {
        json!({
            "host": "github.com",
            "repository": "acme/widgets",
            "number": number,
            "url": format!("https://github.com/acme/widgets/pull/{number}"),
            "source": "manual",
            "linkedAt": linked_at,
            "snapshot": state.map(|state| json!({
                "state": state, "isDraft": false, "headBranch": head, "baseBranch": base,
                "updatedAt": linked_at,
            })),
            "stack": null,
        })
    }

    #[test]
    fn keys_compare_case_insensitively() {
        assert!(thread_pull_request_keys_equal(
            ("GitHub.com", "Acme/Widgets", 7, Some("https://github.com/acme/widgets/pull/7")),
            ("github.com", "acme/widgets", 7, None),
        ));
        assert!(!thread_pull_request_keys_equal(
            ("github.com", "acme/widgets", 7, None),
            ("github.com", "acme/widgets", 8, None),
        ));
    }

    #[test]
    fn current_link_prefers_the_single_open_one() {
        let a = link(1, Some("merged"), "a", "main", "2026-01-01T00:00:00.000Z");
        let b = link(2, Some("open"), "b", "main", "2026-01-02T00:00:00.000Z");
        assert_eq!(resolve_current_link(&[&a, &b]).unwrap()["number"], 2);
    }

    #[test]
    fn current_link_takes_the_top_of_a_stack() {
        let base = link(1, Some("open"), "feature-1", "main", "2026-01-01T00:00:00.000Z");
        let top = link(2, Some("open"), "feature-2", "feature-1", "2026-01-01T00:00:00.000Z");
        let chains = resolve_chains(&[&base, &top]);
        assert_eq!(
            chains,
            vec![Chain {
                native: false,
                layers: vec![0, 1]
            }]
        );
        assert_eq!(resolve_current_link(&[&base, &top]).unwrap()["number"], 2);
    }

    #[test]
    fn legacy_link_needs_a_matching_identity() {
        let open = link(5, None, "x", "main", "2026-01-01T00:00:00.000Z");
        let identity = json!({
            "canonicalKey": "github.com/acme/widgets",
            "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "git@github.com:acme/widgets.git"},
            "displayName": "acme/widgets", "provider": "github", "owner": "acme", "name": "widgets",
        });
        assert_eq!(
            legacy_linked_pull_request_of(std::slice::from_ref(&open), "p1", Some(&identity)),
            Some(json!({"projectId": "p1", "repository": "acme/widgets", "number": 5, "url": "https://github.com/acme/widgets/pull/5"}))
        );
        assert_eq!(legacy_linked_pull_request_of(std::slice::from_ref(&open), "p1", None), None);
        let other = json!({"canonicalKey": "github.com/acme/other", "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "x"}, "displayName": "acme/other", "provider": "github"});
        assert_eq!(legacy_linked_pull_request_of(&[open], "p1", Some(&other)), None);
    }
}
