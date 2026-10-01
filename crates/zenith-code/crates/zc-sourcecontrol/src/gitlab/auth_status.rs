//! `gitLabAuthStatus.ts`: `glab auth status` text, one block per host.

use std::sync::OnceLock;

use regex::Regex;

use crate::util::{js_trim, js_trim_start, split_lines};

/// `GitLabAuthStatusHost`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabAuthStatusHost {
    pub host: String,
    pub account: Option<String>,
}

fn host_line() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)^(?:[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?|\[[a-f0-9:.]+\])(?::[0-9]+)?$").expect("valid regex"))
}

/// `LOGGED_IN_PATTERN`.
pub(crate) fn logged_in() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?i)Logged in to .+? as\s+([^\s(]+)").expect("valid regex"))
}

/// `parseGitLabAuthStatusHosts`.
pub fn parse_gitlab_auth_status_hosts(text: &str) -> Vec<GitLabAuthStatusHost> {
    let mut hosts = Vec::new();
    let mut current: Option<String> = None;
    let mut lines: Vec<String> = Vec::new();
    let mut flush = |current: &mut Option<String>, lines: &mut Vec<String>| {
        if let Some(host) = current.take() {
            let account = logged_in()
                .captures(&lines.join("\n"))
                .and_then(|c| c.get(1).map(|m| js_trim(m.as_str()).to_owned()))
                .filter(|a| !a.is_empty());
            hosts.push(GitLabAuthStatusHost { host, account });
            lines.clear();
        }
    };
    for raw in split_lines(text) {
        let line = js_trim(raw);
        if line.is_empty() {
            continue;
        }
        let is_host_line = raw.len() == js_trim_start(raw).len() && host_line().is_match(line);
        if is_host_line {
            flush(&mut current, &mut lines);
            current = Some(line.to_lowercase());
            continue;
        }
        if current.is_some() {
            lines.push(line.to_owned());
        }
    }
    flush(&mut current, &mut lines);
    hosts
}

/// `findAuthenticatedGitLabHost`.
pub fn find_authenticated_gitlab_host(hosts: &[GitLabAuthStatusHost]) -> Option<&GitLabAuthStatusHost> {
    hosts.iter().find(|host| host.account.is_some())
}
