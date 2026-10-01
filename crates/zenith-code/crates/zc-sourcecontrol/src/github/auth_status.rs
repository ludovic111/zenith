//! `gitHubAuthStatus.ts`: `gh auth status --json hosts`.

use serde_json::Value;

use crate::util::js_trim;

/// `GitHubAuthStatusAccount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubAuthStatusAccount {
    pub host: String,
    pub account: String,
    pub authenticated: bool,
    pub active: bool,
    pub error: Option<String>,
}

/// `GitHubAuthStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitHubAuthStatus {
    pub parsed: bool,
    pub accounts: Vec<GitHubAuthStatusAccount>,
}

struct RawAccount {
    state: String,
    error: Option<String>,
    active: bool,
    host: String,
    login: String,
}

fn decode(text: &str) -> Option<Vec<Vec<RawAccount>>> {
    let value: Value = serde_json::from_str(text).ok()?;
    let hosts = value.as_object()?.get("hosts")?.as_object()?;
    hosts
        .values()
        .map(|accounts| {
            accounts
                .as_array()?
                .iter()
                .map(|account| {
                    let account = account.as_object()?;
                    let error = match account.get("error") {
                        None => None,
                        Some(Value::String(error)) => Some(error.clone()),
                        Some(_) => return None,
                    };
                    Some(RawAccount {
                        state: account.get("state")?.as_str()?.to_owned(),
                        error,
                        active: account.get("active")?.as_bool()?,
                        host: account.get("host")?.as_str()?.to_owned(),
                        login: account.get("login")?.as_str()?.to_owned(),
                    })
                })
                .collect()
        })
        .collect()
}

/// `parseGitHubAuthStatus`.
pub fn parse_github_auth_status(text: &str) -> GitHubAuthStatus {
    let Some(hosts) = decode(text) else {
        return GitHubAuthStatus::default();
    };
    let accounts = hosts
        .into_iter()
        .flatten()
        .filter_map(|account| {
            let host = js_trim(&account.host);
            let login = js_trim(&account.login);
            if host.is_empty() || login.is_empty() {
                return None;
            }
            Some(GitHubAuthStatusAccount {
                host: host.to_lowercase(),
                account: login.to_owned(),
                authenticated: account.state == "success",
                active: account.active,
                error: account.error.as_deref().map(js_trim).filter(|e| !e.is_empty()).map(str::to_owned),
            })
        })
        .collect();
    GitHubAuthStatus { parsed: true, accounts }
}

/// `findAuthenticatedGitHubAccount`: the active authenticated account, else any authenticated one.
pub fn find_authenticated_github_account(accounts: &[GitHubAuthStatusAccount]) -> Option<&GitHubAuthStatusAccount> {
    accounts
        .iter()
        .find(|account| account.authenticated && account.active)
        .or_else(|| accounts.iter().find(|account| account.authenticated))
}
