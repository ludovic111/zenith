//! What a session is, and which zenith code project it belongs to.

use serde::{Deserialize, Serialize};

/// The coding agents whose logs are read.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Codex];

    /// Its product name.
    pub fn name(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }
}

/// A pull request a session opened (Claude Code writes `pr-link` lines).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pr {
    pub number: i64,
    pub url: String,
}

/// A zenith code project folder, as the server knows it: the input that maps sessions to
/// projects. Give one entry per folder; a project with several folders (its threads'
/// worktrees outside the workspace root, say) is listed once per folder with the same `id`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRoot {
    pub id: String,
    pub title: String,
    pub workspace_root: String,
}

/// The project a session was matched to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ProjectRef {
    pub id: String,
    pub title: String,
}

/// One Claude Code or Codex session.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub agent: Agent,
    pub id: String,
    pub title: String,
    /// The zenith code project whose folder holds `cwd` (the deepest one), if any.
    pub project: Option<ProjectRef>,
    pub cwd: String,
    pub branch: Option<String>,
    /// First and last timestamps written, in milliseconds since 1970.
    pub start: i64,
    pub end: i64,
    /// Messages typed by a human.
    pub turns: i64,
    /// The last model used.
    pub model: Option<String>,
    /// Claude Code's own API-equivalent cost (`cost-state` lines). Codex has none.
    #[serde(rename = "costUSD")]
    pub cost_usd: Option<f64>,
    /// Claude: output tokens; Codex: the thread's total tokens.
    pub tokens: Option<i64>,
    pub lines_added: Option<i64>,
    pub lines_removed: Option<i64>,
    pub prs: Vec<Pr>,
    pub subagents: usize,
    /// How it was started (`cli`, `sdk-ts`, `codex_cli_rs`…).
    pub entrypoint: Option<String>,
    /// The shell command that reopens it.
    pub resume: String,
    /// Time really spent writing: 15 minutes per slot with activity.
    pub active_ms: i64,
    /// Written to less than [`LIVE_WINDOW_MS`] ago, when the list was read.
    pub live: bool,
    /// 15-minute slots (epoch / 15 min) where the session wrote something. Kept for Rust
    /// callers (activity charts); not sent over HTTP.
    #[serde(skip)]
    pub slots: Vec<i64>,
}

/// One activity slot: 15 minutes.
pub const SLOT_MS: i64 = 15 * 60_000;

/// A session is live when it wrote something in the last 3 minutes.
pub const LIVE_WINDOW_MS: i64 = 3 * 60_000;

impl Session {
    pub fn is_live_at(&self, now_ms: i64) -> bool {
        now_ms - self.end < LIVE_WINDOW_MS
    }
}

/// The project a folder belongs to: the deepest root holding it.
pub fn project_of<'a>(cwd: &str, roots: &'a [ProjectRoot]) -> Option<&'a ProjectRoot> {
    let mut best: Option<&ProjectRoot> = None;
    for r in roots {
        let root = r.workspace_root.trim_end_matches('/');
        if root.is_empty() {
            continue;
        }
        let inside = cwd == root || cwd.strip_prefix(root).is_some_and(|rest| rest.starts_with('/'));
        if inside && best.is_none_or(|b| root.len() > b.workspace_root.trim_end_matches('/').len()) {
            best = Some(r);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(id: &str, path: &str) -> ProjectRoot {
        ProjectRoot {
            id: id.into(),
            title: id.to_uppercase(),
            workspace_root: path.into(),
        }
    }

    #[test]
    fn attaches_folders_to_the_deepest_project() {
        let r = [
            root("acme", "/p/acme"),
            root("wt", "/p/acme/.claude/worktrees/x/"),
            root("acme", "/w/acme-feature"),
            root("bad", ""),
        ];
        let id = |cwd: &str| project_of(cwd, &r).map(|p| p.id.as_str());
        assert_eq!(id("/p/acme"), Some("acme"));
        assert_eq!(id("/p/acme/web"), Some("acme"));
        assert_eq!(id("/p/acme/.claude/worktrees/x/a"), Some("wt"));
        assert_eq!(id("/w/acme-feature/src"), Some("acme"));
        assert_eq!(id("/p/acme-old"), None);
        assert_eq!(id(""), None);
    }
}
