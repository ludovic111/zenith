//! Line by line readers of the two log formats. Each one is an accumulator fed one JSONL line
//! at a time, so a growing transcript is only read from where the last read stopped
//! ([`crate::reader`]); [`ClaudeLog::session`] and [`CodexLog::session`] turn what was
//! gathered so far into a [`Session`].
//!
//! Transcripts weigh several MB: only lines holding a known marker are parsed as JSON, and
//! the parsed shapes only name the fields used, so serde skips the rest.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use jiff::Timestamp;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use crate::model::{Agent, Pr, Session, SLOT_MS};

/// What a session without a title or prompt is called.
pub const UNTITLED: &str = "Untitled session";

/// Titles taken from a prompt are cut to this many characters.
const TITLE_CHARS: usize = 90;

static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""timestamp":"([^"]+)""#).expect("valid regex"));

pub(crate) fn ms(t: &str) -> Option<i64> {
    t.parse::<Timestamp>().ok().map(|t| t.as_millisecond())
}

/// When a log wrote: first and last timestamps, and every 15-minute slot holding one.
#[derive(Clone, Debug, Default)]
pub(crate) struct Activity {
    first: Option<i64>,
    last: Option<i64>,
    slots: BTreeSet<i64>,
}

impl Activity {
    fn feed(&mut self, line: &str) {
        if !line.contains(r#""timestamp":""#) {
            return;
        }
        for c in TIMESTAMP.captures_iter(line) {
            let Some(t) = ms(&c[1]) else { continue };
            self.first.get_or_insert(t);
            self.last = Some(t);
            self.slots.insert(t.div_euclid(SLOT_MS));
        }
    }

    fn slots(&self) -> Vec<i64> {
        self.slots.iter().copied().collect()
    }

    fn active_ms(&self) -> i64 {
        self.slots.len() as i64 * SLOT_MS
    }
}

/// A prompt as a title: one line, at most [`TITLE_CHARS`] characters.
pub(crate) fn short_title(prompt: &str) -> String {
    let flat = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(TITLE_CHARS) {
        Some((i, _)) => format!("{}…", flat[..i].trim_end()),
        None => flat,
    }
}

/// A JSON value as JavaScript's `String(v ?? "")` would write it.
fn text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}

fn string(v: &Option<Value>) -> Option<&str> {
    v.as_ref().and_then(Value::as_str)
}

fn number(v: &Option<Value>) -> Option<f64> {
    match v.as_ref()? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parses a line only when it holds one of `markers`.
fn parse_if<'a, T: Deserialize<'a>>(line: &'a str, markers: &[&str]) -> Option<T> {
    if markers.iter().any(|m| line.contains(m)) {
        serde_json::from_str(line).ok()
    } else {
        None
    }
}

/// A shell-quoted path, safe in `cd … &&` whatever it holds.
fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-+,:@%".contains(&b)) {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

// --- Claude Code: ~/.claude/projects/<folder>/<session id>.jsonl ---

#[derive(Deserialize)]
struct Usage {
    output_tokens: Option<Value>,
}

#[derive(Deserialize)]
struct Message {
    model: Option<Value>,
    usage: Option<Usage>,
}

/// A line of a Claude Code transcript, as far as zenith cares.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeLine {
    #[serde(rename = "type")]
    kind: Option<Value>,
    custom_title: Option<Value>,
    last_prompt: Option<Value>,
    pr_number: Option<Value>,
    pr_url: Option<Value>,
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<Value>,
    total_lines_added: Option<Value>,
    total_lines_removed: Option<Value>,
    cwd: Option<Value>,
    git_branch: Option<Value>,
    entrypoint: Option<Value>,
    turn_origin: Option<Value>,
    message: Option<Message>,
}

const CLAUDE_MARKERS: &[&str] = &[
    r#""type":"custom-title""#,
    r#""type":"cost-state""#,
    r#""type":"pr-link""#,
    r#""type":"last-prompt""#,
    r#""cwd":""#,
    r#""type":"assistant""#,
];

/// Running state of a Claude Code transcript.
#[derive(Clone, Debug, Default)]
pub struct ClaudeLog {
    activity: Activity,
    title: Option<String>,
    prompt: Option<String>,
    cwd: Option<String>,
    branch: Option<String>,
    entrypoint: Option<String>,
    model: Option<String>,
    /// The last `cost-state` line: cost, lines added, lines removed.
    cost: Option<(Option<f64>, Option<i64>, Option<i64>)>,
    turns: i64,
    output_tokens: i64,
    prs: Vec<Pr>,
}

impl ClaudeLog {
    pub fn feed(&mut self, line: &str) {
        self.activity.feed(line);
        let Some(l) = parse_if::<ClaudeLine>(line, CLAUDE_MARKERS) else {
            return;
        };
        match string(&l.kind) {
            Some("custom-title") => {
                let t = text(l.custom_title.as_ref());
                if !t.is_empty() {
                    self.title = Some(t);
                }
            }
            Some("last-prompt") => {
                let p = text(l.last_prompt.as_ref());
                if !p.is_empty() {
                    self.prompt = Some(p);
                }
            }
            Some("cost-state") => {
                self.cost = Some((
                    number(&l.total_cost_usd),
                    number(&l.total_lines_added).map(|n| n as i64),
                    number(&l.total_lines_removed).map(|n| n as i64),
                ))
            }
            Some("pr-link") => {
                if let Some(n) = number(&l.pr_number) {
                    let url = text(l.pr_url.as_ref());
                    match self.prs.iter_mut().find(|p| p.number == n as i64) {
                        Some(p) => p.url = url,
                        None => self.prs.push(Pr { number: n as i64, url }),
                    }
                }
            }
            Some("assistant") => {
                if let Some(m) = &l.message {
                    if let Some(model) = string(&m.model) {
                        self.model = Some(model.to_owned());
                    }
                    self.output_tokens += m.usage.as_ref().and_then(|u| number(&u.output_tokens)).unwrap_or(0.0) as i64;
                }
            }
            Some("user") if string(&l.turn_origin) == Some("human") => self.turns += 1,
            _ => {}
        }
        if self.cwd.is_none() {
            self.cwd = string(&l.cwd).map(str::to_owned);
        }
        if let Some(b) = string(&l.git_branch).filter(|b| !b.is_empty()) {
            self.branch = Some(b.to_owned());
        }
        if let Some(e) = string(&l.entrypoint) {
            self.entrypoint = Some(e.to_owned());
        }
    }

    /// The session so far, `None` until a timestamp was seen. `project` and `live` are left
    /// for the caller.
    pub fn session(&self, id: &str, subagents: usize) -> Option<Session> {
        let (start, end) = (self.activity.first?, self.activity.last?);
        let cwd = self.cwd.clone().unwrap_or_default();
        let (cost_usd, lines_added, lines_removed) = self.cost.unwrap_or((None, None, None));
        Some(Session {
            agent: Agent::Claude,
            id: id.to_owned(),
            title: self
                .title
                .clone()
                .or_else(|| self.prompt.as_deref().map(short_title))
                .unwrap_or_else(|| UNTITLED.into()),
            project: None,
            resume: format!("cd {} && claude --resume {}", shell_quote(&cwd), shell_quote(id)),
            cwd,
            branch: self.branch.clone(),
            start,
            end,
            turns: self.turns,
            model: self.model.clone(),
            cost_usd,
            tokens: Some(self.output_tokens).filter(|&t| t != 0),
            lines_added,
            lines_removed,
            prs: self.prs.clone(),
            subagents,
            entrypoint: self.entrypoint.clone(),
            active_ms: self.activity.active_ms(),
            live: false,
            slots: self.activity.slots(),
        })
    }
}

// --- Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-….jsonl ---

/// A line of a Codex rollout.
#[derive(Deserialize)]
struct CodexLine {
    #[serde(rename = "type")]
    kind: Option<Value>,
    payload: Option<Value>,
}

const CODEX_MARKERS: &[&str] = &[r#""type":"session_meta""#, r#""type":"turn_context""#, r#""token_count""#, r#""user_message""#];

/// What a rollout's first `session_meta` says.
#[derive(Clone, Debug)]
struct CodexMeta {
    id: Option<String>,
    cwd: String,
    start: Option<i64>,
    branch: Option<String>,
    originator: Option<String>,
}

/// Running state of a Codex rollout.
#[derive(Clone, Debug, Default)]
pub struct CodexLog {
    activity: Activity,
    meta: Option<CodexMeta>,
    model: Option<String>,
    tokens: Option<i64>,
    turns: i64,
    first_prompt: Option<String>,
}

impl CodexLog {
    pub fn feed(&mut self, line: &str) {
        self.activity.feed(line);
        let Some(l) = parse_if::<CodexLine>(line, CODEX_MARKERS) else {
            return;
        };
        let p = l.payload.unwrap_or(Value::Null);
        let inner = p.get("type").and_then(Value::as_str);
        match (string(&l.kind), inner) {
            (Some("session_meta"), _) if self.meta.is_none() => {
                let str_at = |ptr: &str| p.pointer(ptr).and_then(Value::as_str).map(str::to_owned);
                self.meta = Some(CodexMeta {
                    id: str_at("/id"),
                    cwd: text(p.get("cwd")),
                    start: p.get("timestamp").and_then(Value::as_str).and_then(ms),
                    branch: str_at("/git/branch"),
                    originator: str_at("/originator"),
                });
            }
            (Some("turn_context"), _) => {
                if let Some(m) = p.get("model").and_then(Value::as_str) {
                    self.model = Some(m.to_owned());
                }
            }
            (Some("event_msg"), Some("token_count")) => {
                if let Some(t) = p.pointer("/info/total_token_usage/total_tokens").and_then(Value::as_i64) {
                    self.tokens = Some(t);
                }
            }
            (Some("event_msg"), Some("user_message")) => {
                self.turns += 1;
                if self.first_prompt.is_none() {
                    self.first_prompt = p.get("message").and_then(Value::as_str).map(str::to_owned);
                }
            }
            _ => {}
        }
    }

    /// The thread id, once `session_meta` was read.
    pub fn id(&self) -> Option<&str> {
        self.meta.as_ref()?.id.as_deref()
    }

    /// The session so far, `None` without a `session_meta`. `titles` are the thread names
    /// of `session_index.jsonl`.
    pub fn session(&self, thread_name: Option<&str>) -> Option<Session> {
        let meta = self.meta.as_ref()?;
        let id = meta.id.clone()?;
        let start = meta.start?;
        let end = self.activity.last.unwrap_or(start).max(start);
        Some(Session {
            agent: Agent::Codex,
            title: thread_name
                .map(str::to_owned)
                .or_else(|| self.first_prompt.as_deref().map(short_title))
                .unwrap_or_else(|| UNTITLED.into()),
            project: None,
            cwd: meta.cwd.clone(),
            branch: meta.branch.clone(),
            start,
            end,
            turns: self.turns,
            model: self.model.clone(),
            cost_usd: None,
            tokens: self.tokens,
            lines_added: None,
            lines_removed: None,
            prs: Vec::new(),
            subagents: 0,
            entrypoint: meta.originator.clone(),
            resume: format!("codex resume {}", shell_quote(&id)),
            id,
            active_ms: self.activity.active_ms(),
            live: false,
            slots: self.activity.slots(),
        })
    }
}

/// Thread names from `~/.codex/session_index.jsonl`; a later line wins.
pub fn codex_titles(index: &str) -> std::collections::HashMap<String, String> {
    let mut titles = std::collections::HashMap::new();
    for line in index.lines().filter(|l| !l.is_empty()) {
        let Ok(d) = serde_json::from_str::<Value>(line) else { continue };
        let Some(id) = d.get("id").and_then(Value::as_str) else { continue };
        match d.get("thread_name").and_then(Value::as_str).filter(|t| !t.is_empty()) {
            Some(t) => titles.insert(id.to_owned(), t.to_owned()),
            None => titles.remove(id),
        };
    }
    titles
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const CLAUDE: &str = r#"{"type":"user","cwd":"/p/acme/web","gitBranch":"main","entrypoint":"cli","turnOrigin":"human","timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"hi"}}
{"type":"assistant","cwd":"/p/acme","gitBranch":"","entrypoint":"cli","timestamp":"2026-10-01T10:01:00.000Z","message":{"model":"claude-opus-5-5","content":[{"type":"text","text":"\"timestamp\":\"nope\""}],"usage":{"output_tokens":120}}}
{"type":"last-prompt","lastPrompt":"port the   widget\nto Rust","sessionId":"s1"}
{"type":"pr-link","prNumber":4,"prUrl":"https://github.com/o/r/pull/4","timestamp":"2026-10-01T10:20:00.000Z"}
{"type":"pr-link","prNumber":4,"prUrl":"https://github.com/o/r/pull/4b","timestamp":"2026-10-01T10:21:00.000Z"}
{"type":"user","cwd":"/p/acme","gitBranch":"feat","entrypoint":"sdk-ts","turnOrigin":"tool","timestamp":"2026-10-01T10:31:00.000Z","message":{"role":"user","content":[]}}
{"type":"cost-state","totalCostUSD":1.25,"totalLinesAdded":10,"totalLinesRemoved":2}
{"type":"assistant","cwd":"/p/acme","timestamp":"2026-10-01T10:32:00.000Z","message":{"model":"claude-sonnet-5","usage":{"output_tokens":30}}}
not json "cwd":"
"#;

    pub fn claude(raw: &str) -> ClaudeLog {
        let mut log = ClaudeLog::default();
        raw.lines().for_each(|l| log.feed(l));
        log
    }

    pub fn codex(raw: &str) -> CodexLog {
        let mut log = CodexLog::default();
        raw.lines().for_each(|l| log.feed(l));
        log
    }

    #[test]
    fn reads_a_claude_transcript() {
        let s = claude(CLAUDE).session("s1", 2).unwrap();
        assert_eq!(s.title, "port the widget to Rust");
        assert_eq!(s.cwd, "/p/acme/web");
        assert_eq!(s.branch.as_deref(), Some("feat"));
        assert_eq!(s.entrypoint.as_deref(), Some("sdk-ts"));
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!((s.turns, s.tokens, s.subagents), (1, Some(150), 2));
        assert_eq!((s.cost_usd, s.lines_added, s.lines_removed), (Some(1.25), Some(10), Some(2)));
        assert_eq!(
            s.prs,
            [Pr {
                number: 4,
                url: "https://github.com/o/r/pull/4b".into()
            }]
        );
        assert_eq!(s.start, ms("2026-10-01T10:00:00Z").unwrap());
        assert_eq!(s.end, ms("2026-10-01T10:32:00Z").unwrap());
        // 10:00, 10:01 → one slot; 10:20, 10:21 → another; 10:31, 10:32 → a third.
        assert_eq!(s.slots.len(), 3);
        assert_eq!(s.active_ms, 45 * 60_000);
        assert_eq!(s.resume, "cd /p/acme/web && claude --resume s1");
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["costUSD"], 1.25);
        assert_eq!(json["linesAdded"], 10);
        assert_eq!(json["agent"], "claude");
        assert_eq!(json["activeMs"], 45 * 60_000);
        assert!(json.get("slots").is_none());
    }

    #[test]
    fn titles_win_over_prompts() {
        let raw = format!("{CLAUDE}{}\n", r#"{"type":"custom-title","customTitle":"Port to Rust"}"#);
        assert_eq!(claude(&raw).session("s1", 0).unwrap().title, "Port to Rust");
        assert!(claude(r#"{"type":"custom-title","customTitle":"x"}"#).session("s", 0).is_none());
        let bare = r#"{"type":"user","cwd":"/p/it's here","timestamp":"2026-10-01T10:00:00.000Z"}"#;
        let s = claude(bare).session("s2", 0).unwrap();
        assert_eq!(s.title, UNTITLED);
        assert_eq!(s.resume, r"cd '/p/it'\''s here' && claude --resume s2");
    }

    #[test]
    fn long_prompts_are_cut() {
        let t = short_title(&"word ".repeat(40));
        assert_eq!(t.chars().count(), 90);
        assert!(t.ends_with('…'));
        assert_eq!(short_title("  a\n b "), "a b");
    }

    pub const CODEX: &str = r#"{"timestamp":"2026-10-01T08:50:48.469Z","type":"session_meta","payload":{"id":"c1","timestamp":"2026-10-01T08:50:48.376Z","cwd":"/p/zen","originator":"codex_cli_rs","git":{"branch":"main"}}}
{"timestamp":"2026-10-01T08:50:52.067Z","type":"turn_context","payload":{"model":"gpt-5.5"}}
{"timestamp":"2026-10-01T08:50:53.000Z","type":"event_msg","payload":{"type":"user_message","message":"tidy the desk"}}
{"timestamp":"2026-10-01T08:50:58.992Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":18566}}}}
{"timestamp":"2026-10-01T09:20:00.000Z","type":"event_msg","payload":{"type":"token_count","info":null}}
"#;

    #[test]
    fn reads_a_codex_rollout() {
        let log = codex(CODEX);
        assert_eq!(log.id(), Some("c1"));
        let s = log.session(None).unwrap();
        assert_eq!((s.id.as_str(), s.title.as_str()), ("c1", "tidy the desk"));
        assert_eq!((s.model.as_deref(), s.tokens, s.turns), (Some("gpt-5.5"), Some(18566), 1));
        assert_eq!((s.branch.as_deref(), s.entrypoint.as_deref()), (Some("main"), Some("codex_cli_rs")));
        assert_eq!(s.start, ms("2026-10-01T08:50:48.376Z").unwrap());
        assert_eq!(s.end, ms("2026-10-01T09:20:00Z").unwrap());
        assert_eq!(s.resume, "codex resume c1");
        assert_eq!(log.session(Some("Tidy the desk")).unwrap().title, "Tidy the desk");
        assert!(codex(r#"{"timestamp":"2026-10-01T08:50:52.067Z","type":"turn_context","payload":{}}"#)
            .session(None)
            .is_none());
    }

    #[test]
    fn later_thread_names_win() {
        let titles = codex_titles("{\"id\":\"c1\",\"thread_name\":\"Old\"}\n{\"id\":\"c1\",\"thread_name\":\"New\"}\nnot json\n{\"id\":\"c2\"}\n");
        assert_eq!(titles.get("c1").map(String::as_str), Some("New"));
        assert!(!titles.contains_key("c2"));
    }
}
