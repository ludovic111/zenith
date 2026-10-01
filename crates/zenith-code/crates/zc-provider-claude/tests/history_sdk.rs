//! `getSessionMessages` / `forkSession` as direct JSONL operations, checked against the Agent
//! SDK (0.3.276) itself.
//!
//! By default the test runs on `tests/fixtures/history-home` (synthetic transcripts: compaction,
//! sidechains, queued task notifications, local commands, rewound branches) against
//! `tests/fixtures/sdk-history.json`, which the SDK produced from a copy of that directory:
//!   cp -R tests/fixtures/history-home /tmp/h && CLAUDE_CONFIG_DIR=/tmp/h \
//!     node tests/fixtures/sdk-history.mjs <sdk dir> tests/fixtures/sdk-history.json
//! Real transcripts are personal data and never committed; to check some, copy session files
//! into a scratch config dir (`<dir>/projects/<project>/<id>.jsonl`), generate the expectations
//! from a second copy the same way, then run with
//!   ZC_CLAUDE_HISTORY_HOME=<pristine copy> ZC_CLAUDE_HISTORY_SDK=<out.json>.
//! The Rust side always works on its own temporary copy.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value};
use zc_provider_claude::history::{fork_session, get_session_messages};

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// Renumbers UUIDs by first appearance and drops the fields a fork stamps with the clock.
struct Normalizer {
    regex: regex::Regex,
    ids: HashMap<String, usize>,
}

impl Normalizer {
    fn new() -> Self {
        Self {
            regex: regex::Regex::new(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap(),
            ids: HashMap::new(),
        }
    }

    fn apply(&mut self, value: &Value) -> Value {
        match value {
            Value::String(text) => {
                let ids = &mut self.ids;
                Value::String(
                    self.regex
                        .replace_all(text, |captures: &regex::Captures<'_>| {
                            let next = ids.len();
                            format!("<id{}>", ids.entry(captures[0].to_string()).or_insert(next))
                        })
                        .into_owned(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(|item| self.apply(item)).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), self.apply(v))).collect::<Map<_, _>>()),
            _ => value.clone(),
        }
    }
}

/// The paths where two values differ (at most a few), with both sides shortened.
fn diff_paths(path: &str, left: Option<&Value>, right: Option<&Value>, out: &mut Vec<String>) {
    if out.len() >= 6 || left == right {
        return;
    }
    match (left, right) {
        (Some(Value::Object(a)), Some(Value::Object(b))) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                diff_paths(&format!("{path}.{key}"), a.get(key), b.get(key), out);
            }
        }
        (Some(Value::Array(a)), Some(Value::Array(b))) if a.len() == b.len() => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                diff_paths(&format!("{path}[{index}]"), Some(x), Some(y), out);
            }
        }
        _ => {
            let show = |v: Option<&Value>| {
                v.map(|v| v.to_string().chars().take(160).collect::<String>())
                    .unwrap_or_else(|| "<absent>".into())
            };
            out.push(format!("{path}: sdk={} rust={}", show(left), show(right)));
        }
    }
}

fn first_difference(left: &[Value], right: &[Value]) -> Option<String> {
    let index = left
        .iter()
        .zip(right)
        .position(|(a, b)| a != b)
        .or((left.len() != right.len()).then(|| left.len().min(right.len())))?;
    let mut paths = Vec::new();
    diff_paths("", left.get(index), right.get(index), &mut paths);
    Some(format!("#{index} of {}/{}: {}", left.len(), right.len(), paths.join(" | ")))
}

#[test]
fn matches_the_sdk_on_recorded_transcripts() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let (home, expected) = match (std::env::var("ZC_CLAUDE_HISTORY_HOME"), std::env::var("ZC_CLAUDE_HISTORY_SDK")) {
        (Ok(home), Ok(expected)) => (home, expected),
        _ => (
            fixtures.join("history-home").display().to_string(),
            fixtures.join("sdk-history.json").display().to_string(),
        ),
    };
    let temp = tempfile::tempdir().unwrap();
    copy_dir(Path::new(&home), temp.path());
    let config = temp.path();
    let expected: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(expected).unwrap()).unwrap();
    let mut failures = 0;
    let (mut messages_checked, mut forks_checked, mut lines_checked) = (0, 0, 0);
    for case in &expected {
        let session = case["sessionId"].as_str().unwrap();
        let mut report = Vec::new();
        for (key, include_system) in [("messages", false), ("withSystem", true)] {
            let sdk: Vec<Value> = case[key].as_array().unwrap().clone();
            let ours = get_session_messages(config, session, None, include_system);
            messages_checked += sdk.len();
            if let Some(diff) = first_difference(&sdk, &ours) {
                report.push(format!("  getSessionMessages({key}) differs at {diff}"));
            }
        }
        if let Some(fork) = case["fork"].as_object() {
            let up_to = fork["upTo"].as_str().unwrap();
            let fork_id = fork_session(config, session, None, Some(up_to), None).unwrap();
            let dir = case["dir"].as_str().unwrap();
            let file = config.join("projects").join(dir).join(format!("{fork_id}.jsonl"));
            let ours: Vec<Value> = std::fs::read_to_string(&file)
                .unwrap()
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            let (mut left, mut right) = (Normalizer::new(), Normalizer::new());
            // The new session id and fresh message uuids line up by first appearance; the
            // original session's ids appear in both in the same order.
            // Entries a fork stamps with the current time (later than anything in the original)
            // were written a minute apart by the SDK and by us.
            let original = std::fs::read_to_string(Path::new(&home).join("projects").join(dir).join(format!("{session}.jsonl"))).unwrap();
            let latest = original
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter_map(|v| v["timestamp"].as_str().map(str::to_string))
                .max()
                .unwrap_or_default();
            let stamp = |line: &Value| {
                let mut line = line.clone();
                if line["timestamp"].as_str().is_some_and(|t| t > latest.as_str()) {
                    line["timestamp"] = Value::String("<fork time>".into());
                }
                line
            };
            let sdk_lines: Vec<Value> = fork["lines"].as_array().unwrap().iter().map(|l| left.apply(&stamp(l))).collect();
            let our_lines: Vec<Value> = ours.iter().map(|l| right.apply(&stamp(l))).collect();
            lines_checked += sdk_lines.len();
            forks_checked += 1;
            if let Some(diff) = first_difference(&sdk_lines, &our_lines) {
                report.push(format!("  forkSession transcript differs at {diff}"));
            }
            let sdk_messages: Vec<Value> = fork["messages"].as_array().unwrap().iter().map(|m| left.apply(&stamp(m))).collect();
            let our_messages: Vec<Value> = get_session_messages(config, &fork_id, None, true)
                .iter()
                .map(|m| right.apply(&stamp(m)))
                .collect();
            if let Some(diff) = first_difference(&sdk_messages, &our_messages) {
                report.push(format!("  messages of the fork differ at {diff}"));
            }
        }
        if report.is_empty() {
            eprintln!("MATCH {session}");
        } else {
            failures += 1;
            eprintln!("DIFF  {session}\n{}", report.join("\n"));
        }
    }
    eprintln!(
        "history: {}/{} sessions identical ({messages_checked} messages, {forks_checked} forks with {lines_checked} transcript lines)",
        expected.len() - failures,
        expected.len()
    );
    assert_eq!(failures, 0);
}
