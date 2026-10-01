//! Golden comparisons against the TypeScript server, run from source with node:
//!
//! 1. Repository identities: a set of temp repositories (GitHub, GitLab groups, Azure,
//!    Bitbucket, an unknown host, upstream over origin, a nested folder, no remote, no
//!    repository) resolved by the TS `RepositoryIdentityResolver` and by
//!    [`zc_project::RepositoryIdentities`] must be identical.
//! 2. Agent sessions: the TS `AgentSessionScanner` and [`zc_project::AgentSessionScanner`] scan
//!    the real `~/.claude` and `~/.codex` (or `CLAUDE_CONFIG_DIR` / `CODEX_HOME`), **read
//!    only**, with the default provider settings, the same fixed clock and the same project
//!    roots; the scan results and the recent sessions of a few stable projects must match.
//!    Transcripts being written right now (this very session, for one) are normalized.
//!
//! Needs `node` and `code/apps/server/node_modules` (symlinked from a checkout that has them);
//! without them the tests print why and pass vacuously.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_project::sessions::{RecentThread, ScannerConfig, StaticSettings};
use zc_project::{AgentSessionScanner, RepositoryIdentities};

fn server_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../code/apps/server")
}

fn oracle_available() -> Result<(), String> {
    if !server_dir().join("node_modules/effect").exists() {
        return Err(format!("{} has no node_modules", server_dir().display()));
    }
    match Command::new("node").arg("--version").output() {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("node is not installed".into()),
    }
}

fn run_oracle(script: &str, request: &Value) -> Value {
    let script = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(script)).unwrap();
    let server = std::fs::canonicalize(server_dir()).unwrap();
    let mut child = Command::new("node")
        .args(["--no-warnings", "--input-type=module", "-e", &script])
        .current_dir(&server)
        .env("ZC_SERVER_SRC", server.join("src"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(request.to_string().as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "oracle failed: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json = stdout.rsplit("@@ORACLE@@").next().unwrap_or_default();
    serde_json::from_str(json).unwrap_or_else(|e| panic!("oracle output: {e}: {}", &stdout[..stdout.len().min(2000)]))
}

#[tokio::test]
async fn repository_identities_match_the_typescript_resolver() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipped: {reason}");
        return;
    }
    let (_guard, root) = temp_dir("zc-project-identity-");
    let repos: Vec<(&str, Vec<Vec<&str>>)> = vec![
        ("github-ssh", vec![vec!["remote", "add", "origin", "git@github.com:Octo-Org/sample-app.git"]]),
        (
            "github-https",
            vec![vec!["remote", "add", "origin", "https://github.com/Octo-Org/Sample-App.git/"]],
        ),
        (
            "gitlab-group",
            vec![vec!["remote", "add", "origin", "git@gitlab.com:Octo-Org/platform/sample-app.git"]],
        ),
        (
            "azure",
            vec![vec!["remote", "add", "origin", "https://dev.azure.com/octo-org/platform/_git/sample-app"]],
        ),
        (
            "azure-ssh",
            vec![vec!["remote", "add", "origin", "git@ssh.dev.azure.com:v3/octo-org/platform/sample-app"]],
        ),
        ("bitbucket", vec![vec!["remote", "add", "origin", "git@bitbucket.org:octo-org/sample-app.git"]]),
        (
            "unknown-host",
            vec![vec!["remote", "add", "origin", "ssh://git@code.example.test:2222/team/sample-app.git"]],
        ),
        (
            "self-hosted",
            vec![vec!["remote", "add", "origin", "https://gitlab.example.test/team/sample-app.git"]],
        ),
        (
            "upstream",
            vec![
                vec!["remote", "add", "origin", "git@github.com:someone/sample-app.git"],
                vec!["remote", "add", "upstream", "git@github.com:Octo-Org/sample-app.git"],
            ],
        ),
        (
            "sorted",
            vec![
                vec!["remote", "add", "zeta", "git@github.com:zeta/sample-app.git"],
                vec!["remote", "add", "alpha", "git@github.com:alpha/sample-app.git"],
            ],
        ),
        ("local-path", vec![vec!["remote", "add", "origin", "/srv/git/sample-app.git"]]),
        ("no-remote", vec![]),
    ];
    let mut cwds = Vec::new();
    for (name, commands) in &repos {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]);
        for command in commands {
            git(&dir, command);
        }
        cwds.push(dir.to_string_lossy().into_owned());
    }
    let nested = root.join("github-ssh").join("packages").join("web");
    std::fs::create_dir_all(&nested).unwrap();
    cwds.push(nested.to_string_lossy().into_owned());
    let plain = root.join("not-a-repo");
    std::fs::create_dir_all(&plain).unwrap();
    cwds.push(plain.to_string_lossy().into_owned());
    cwds.push(root.join("missing").to_string_lossy().into_owned());

    let expected = run_oracle("identity_oracle.mjs", &json!({ "cwds": cwds }));
    let resolver = RepositoryIdentities::default();
    let mut actual = Vec::new();
    for cwd in &cwds {
        actual.push(serde_json::to_value(resolver.resolve(cwd, false).await).unwrap());
    }
    let expected = expected.as_array().unwrap();
    assert_eq!(expected.len(), actual.len());
    for ((cwd, ts), rust) in cwds.iter().zip(expected).zip(&actual) {
        assert_eq!(rust, ts, "identity of {cwd}");
    }
    assert!(expected.iter().filter(|v| !v.is_null()).count() >= 11);
}

/// The JSON the oracle prints for an outcome.
fn outcome_json(outcome: &RecentThread) -> Value {
    let thread_json = |thread: &zc_project::sessions::AgentSessionThread| {
        json!({
            "source": thread.source,
            "providerInstanceId": thread.provider_instance_id,
            "providerSessionId": thread.provider_session_id,
            "title": thread.title,
            "model": thread.model,
            "createdAt": thread.created_at,
            "updatedAt": thread.updated_at,
            "messages": thread.messages.iter().map(|m| json!({"role": m.role, "text": m.text, "createdAt": m.created_at})).collect::<Vec<_>>(),
        })
    };
    match outcome {
        RecentThread::Importable { thread, source } => json!({"tag": "Importable", "source": source, "thread": thread_json(thread)}),
        RecentThread::AlreadyImported { source } => json!({"tag": "AlreadyImported", "source": source}),
        RecentThread::Duplicate { source } => json!({"tag": "Duplicate", "source": source}),
        RecentThread::Skipped => json!({"tag": "Skipped"}),
    }
}

/// A short, content-free description of a difference (transcripts are private).
fn describe(value: &Value) -> String {
    match value {
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter().map(|(k, v)| format!("{k}: {}", describe(v))).collect::<Vec<_>>().join(", ")
        ),
        Value::Array(items) => format!("[{} items]", items.len()),
        // Timestamps are not content: show them.
        Value::String(text) if zc_core::time::parse_iso_millis(text).is_some() => text.clone(),
        Value::String(text) => format!("<{} chars>", text.chars().count()),
        other => other.to_string(),
    }
}

fn first_difference(path: &str, left: &Value, right: &Value) -> Option<String> {
    match (left, right) {
        (Value::Object(l), Value::Object(r)) => {
            for key in l.keys().chain(r.keys()) {
                let (a, b) = (l.get(key).unwrap_or(&Value::Null), r.get(key).unwrap_or(&Value::Null));
                if let Some(diff) = first_difference(&format!("{path}.{key}"), a, b) {
                    return Some(diff);
                }
            }
            None
        }
        (Value::Array(l), Value::Array(r)) => {
            if l.len() != r.len() {
                return Some(format!("{path}: {} vs {} items", l.len(), r.len()));
            }
            l.iter()
                .zip(r)
                .enumerate()
                .find_map(|(i, (a, b))| first_difference(&format!("{path}[{i}]"), a, b))
        }
        (a, b) if a == b => None,
        (a, b) => Some(format!("{path}: {} vs {}", describe(a), describe(b))),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_session_scan_matches_the_typescript_scanner_on_the_real_homes() {
    if let Err(reason) = oracle_available() {
        eprintln!("skipped: {reason}");
        return;
    }
    let (_guard, base_dir) = temp_dir("zc-project-scan-base-");
    let now_ms = zc_core::now_millis();
    let settings = json!({
        "providers": {"claudeAgent": {"homePath": ""}, "codex": {"homePath": "", "shadowHomePath": ""}},
        "providerInstances": {},
    });
    let make_scanner = |reads| {
        let mut config = ScannerConfig::new(&base_dir, base_dir.join("worktrees"));
        config.now_millis = Arc::new(move || now_ms);
        AgentSessionScanner::new(config, Arc::new(StaticSettings(settings.clone())), reads)
    };

    // A first Rust scan picks the project roots both sides then know about.
    let empty = stack().await;
    let first = make_scanner(empty.reads.clone()).scan().await.unwrap();
    let paths: Vec<String> = first.candidates.iter().map(|c| c.path.clone()).collect();
    let projects: Vec<String> = paths.iter().take(2).cloned().collect();
    // Recent sessions of projects not touched in the last hour (stable transcripts).
    let hour_ago = now_ms - 3_600_000;
    let recent: Vec<String> = first
        .candidates
        .iter()
        .filter(|c| {
            c.last_active_at
                .as_deref()
                .and_then(zc_core::time::parse_iso_millis)
                .is_some_and(|ms| ms < hour_ago && ms > now_ms - 25 * 24 * 3_600_000)
        })
        .take(8)
        .map(|c| c.path.clone())
        .collect();

    let started = std::time::Instant::now();
    let expected = run_oracle(
        "scanner_oracle.mjs",
        &json!({"baseDir": base_dir, "nowMs": now_ms, "projects": projects, "recent": recent}),
    );

    eprintln!("TS scanner: {:?}", started.elapsed());
    let started = std::time::Instant::now();
    let stack = stack().await;
    for (index, root) in projects.iter().enumerate() {
        stack.create_project(&format!("project-{}", index + 1), root).await;
    }
    let scanner = make_scanner(stack.reads.clone());
    let scan = scanner.scan().await.unwrap();
    let mut actual = json!({ "scan": scan, "recent": {} });
    for root in &recent {
        let outcomes: Vec<Value> = scanner
            .recent_threads(root, Vec::new())
            .await
            .unwrap()
            .map(|o| outcome_json(&o))
            .collect()
            .await;
        actual["recent"][root] = Value::Array(outcomes);
    }

    eprintln!("Rust scanner: {:?}", started.elapsed());
    // What legitimately differs: the scan time, and candidates whose transcripts are being
    // written right now (their dates and counts move between the two runs).
    let recent_cutoff = now_ms - 10 * 60_000;
    let normalize = |value: &mut Value| {
        value["scan"]["scannedAt"] = json!("<time>");
        if let Some(candidates) = value["scan"]["candidates"].as_array_mut() {
            candidates.retain(|c| {
                c["lastActiveAt"]
                    .as_str()
                    .and_then(zc_core::time::parse_iso_millis)
                    .is_none_or(|ms| ms < recent_cutoff)
            });
        }
    };
    let mut expected = expected;
    normalize(&mut expected);
    normalize(&mut actual);
    let ts_count = expected["scan"]["candidates"].as_array().map_or(0, Vec::len);
    let recent_count: usize = expected["recent"]
        .as_object()
        .map_or(0, |m| m.values().map(|v| v.as_array().map_or(0, Vec::len)).sum());
    eprintln!(
        "compared {ts_count} stable candidates and {recent_count} recent-session outcomes across {} projects",
        recent.len()
    );
    if let Some(diff) = first_difference("$", &actual, &expected) {
        panic!("Rust and TS scanners differ at {diff}");
    }
}
