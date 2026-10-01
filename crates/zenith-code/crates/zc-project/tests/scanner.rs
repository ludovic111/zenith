//! Port of the `scan` and `recentThreads` parts of `project/AgentSessionScanner.test.ts`.
//!
//! The TS tests swap Effect's `FileSystem` to count opens and reads or to alias paths; here
//! the scanner's [`ScanFileSystem`] is wrapped the same way ([`TestFs`]). Homes, workspaces and
//! the user's home directory are temp directories (the TS test that writes into the real
//! `~/Documents/Codex` and `~/Downloads` uses a fake home here).

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{AgentSessionImportSource, AgentSessionScanResult, AgentSessionSource};
use zc_project::sessions::fs::{FileStat, RealFileSystem, ScanFile, ScanFileSystem};
use zc_project::sessions::{RecentThread, ScannerConfig, StaticSettings};
use zc_project::AgentSessionScanner;

const NOW_MS: i64 = 1_787_572_800_000; // 2026-08-24T12:00:00.000Z
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn ms(iso: &str) -> i64 {
    zc_core::time::parse_iso_millis(iso).unwrap()
}

fn write_transcript(file_path: &Path, contents: &str, mtime_ms: i64) {
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(file_path, contents).unwrap();
    set_mtime(file_path, mtime_ms);
}

fn set_mtime(file_path: &Path, mtime_ms: i64) {
    let file = std::fs::File::options().write(true).open(file_path).unwrap();
    file.set_modified(UNIX_EPOCH + Duration::from_millis(mtime_ms as u64)).unwrap();
}

fn record(value: Value) -> String {
    value.to_string()
}

fn claude_session_line(cwd: &Path) -> String {
    format!(
        "{}\n{}\n",
        record(json!({"type": "user", "cwd": cwd, "sessionId": "s1"})),
        record(json!({"type": "assistant"}))
    )
}

fn codex_rollout_line(cwd: &Path) -> String {
    format!(
        "{}\n",
        record(json!({"timestamp": "2026-01-01T00:00:00.000Z", "type": "session_meta", "payload": {"id": "r1", "cwd": cwd}}))
    )
}

fn codex_session(id: &str, cwd: &Path, prompt: &str) -> String {
    [
        record(json!({"type": "session_meta", "payload": {"id": id, "cwd": cwd}})),
        record(json!({"type": "event_msg", "payload": {"type": "user_message", "message": prompt}})),
    ]
    .join("\n")
}

fn rollout(home: &Path, day: &str, name: &str) -> PathBuf {
    let parts: Vec<&str> = day.split('-').collect();
    home.join("sessions").join(parts[0]).join(parts[1]).join(parts[2]).join(name)
}

fn mkdir(prefix: &str) -> (tempfile::TempDir, PathBuf) {
    // Not canonicalized: the TS tests use the temp paths as spelled.
    let dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn basename(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

type DirHook = Arc<dyn Fn(&Path) -> Option<std::io::Result<Vec<String>>> + Send + Sync>;
type StatHook = Arc<dyn Fn(&Path) -> Option<std::io::Result<FileStat>> + Send + Sync>;
type OpenHook = Arc<dyn Fn(&Path) -> Option<std::io::Result<Box<dyn ScanFile>>> + Send + Sync>;

/// The real file system with optional per-call overrides (each returns `None` to fall through).
#[derive(Clone, Default)]
struct TestFs {
    dir: Option<DirHook>,
    stat: Option<StatHook>,
    open: Option<OpenHook>,
}

impl ScanFileSystem for TestFs {
    fn read_directory(&self, directory: &Path) -> std::io::Result<Vec<String>> {
        if let Some(result) = self.dir.as_ref().and_then(|hook| hook(directory)) {
            return result;
        }
        RealFileSystem.read_directory(directory)
    }
    fn stat(&self, path: &Path) -> std::io::Result<FileStat> {
        if let Some(result) = self.stat.as_ref().and_then(|hook| hook(path)) {
            return result;
        }
        RealFileSystem.stat(path)
    }
    fn real_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        RealFileSystem.real_path(path)
    }
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        RealFileSystem.read_file(path)
    }
    fn open(&self, path: &Path) -> std::io::Result<Box<dyn ScanFile>> {
        if let Some(result) = self.open.as_ref().and_then(|hook| hook(path)) {
            return result;
        }
        RealFileSystem.open(path)
    }
}

type OnRead = Arc<dyn Fn(usize, &Option<Vec<u8>>) + Send + Sync>;

/// A file whose reads are observed (and possibly replaced).
struct ObservedFile {
    inner: Box<dyn ScanFile>,
    on_read: OnRead,
}

impl ScanFile for ObservedFile {
    fn read_alloc(&mut self, size: usize) -> std::io::Result<Option<Vec<u8>>> {
        let chunk = self.inner.read_alloc(size)?;
        (self.on_read)(size, &chunk);
        Ok(chunk)
    }
    fn stat(&self) -> std::io::Result<FileStat> {
        self.inner.stat()
    }
}

/// A file serving `bytes` one byte per read.
struct ByteByByte {
    bytes: Vec<u8>,
    offset: usize,
    reads: Arc<AtomicUsize>,
    stat: FileStat,
}

impl ScanFile for ByteByByte {
    fn read_alloc(&mut self, _size: usize) -> std::io::Result<Option<Vec<u8>>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.offset == self.bytes.len() {
            return Ok(None);
        }
        self.offset += 1;
        Ok(Some(vec![self.bytes[self.offset - 1]]))
    }
    fn stat(&self) -> std::io::Result<FileStat> {
        Ok(self.stat)
    }
}

struct Input {
    claude: PathBuf,
    codex: PathBuf,
    base_dir: Option<PathBuf>,
    home: Option<PathBuf>,
    provider_instances: Value,
    imported: Vec<PathBuf>,
    fs: TestFs,
}

impl Input {
    fn new(claude: &Path, codex: &Path) -> Self {
        Self {
            claude: claude.to_path_buf(),
            codex: codex.to_path_buf(),
            base_dir: None,
            home: None,
            provider_instances: json!({}),
            imported: Vec::new(),
            fs: TestFs::default(),
        }
    }
}

struct Harness {
    scanner: AgentSessionScanner,
    _stack: Stack,
    _dirs: Vec<tempfile::TempDir>,
}

async fn harness(input: Input) -> Harness {
    let mut dirs = Vec::new();
    let base_dir = input.base_dir.clone().unwrap_or_else(|| {
        let (dir, path) = mkdir("scanner-config-");
        dirs.push(dir);
        path
    });
    let home = input.home.clone().unwrap_or_else(|| {
        let (dir, path) = mkdir("scanner-home-");
        dirs.push(dir);
        path
    });
    let stack = stack().await;
    for (index, root) in input.imported.iter().enumerate() {
        stack.create_project(&format!("project-{}", index + 1), &root.to_string_lossy()).await;
    }
    let mut config = ScannerConfig::new(&base_dir, base_dir.join("worktrees"));
    config.home_dir = home;
    config.environment = HashMap::new();
    config.now_millis = Arc::new(|| NOW_MS);
    config.fs = Arc::new(input.fs);
    let settings = json!({
        "providers": {
            "claudeAgent": {"homePath": input.claude},
            "codex": {"homePath": input.codex, "shadowHomePath": ""},
        },
        "providerInstances": input.provider_instances,
    });
    Harness {
        scanner: AgentSessionScanner::new(config, Arc::new(StaticSettings(settings)), stack.reads.clone()),
        _stack: stack,
        _dirs: dirs,
    }
}

async fn run_scan(input: Input) -> AgentSessionScanResult {
    harness(input).await.scanner.scan().await.unwrap()
}

async fn run_outcomes(input: Input, root: &Path) -> Vec<RecentThread> {
    let harness = harness(input).await;
    harness
        .scanner
        .recent_threads(&root.to_string_lossy(), Vec::new())
        .await
        .unwrap()
        .collect()
        .await
}

fn tags(outcomes: &[RecentThread]) -> Vec<&'static str> {
    outcomes
        .iter()
        .map(|outcome| match outcome {
            RecentThread::Importable { .. } => "Importable",
            RecentThread::AlreadyImported { .. } => "AlreadyImported",
            RecentThread::Duplicate { .. } => "Duplicate",
            RecentThread::Skipped => "Skipped",
        })
        .collect()
}

fn importable(outcomes: &[RecentThread]) -> Vec<zc_project::sessions::AgentSessionThread> {
    outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            RecentThread::Importable { thread, .. } => Some(thread.clone()),
            _ => None,
        })
        .collect()
}

fn paths(result: &AgentSessionScanResult) -> Vec<String> {
    result.candidates.iter().map(|c| c.path.clone()).collect()
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn candidate(path: &Path, sources: &[&str], count: i64, last: &str, imported: bool, git: Value) -> Value {
    let mut value = json!({
        "path": path, "title": basename(path), "sources": sources, "threadCount": count,
        "lastActiveAt": last, "alreadyImported": imported, "git": git,
    });
    if imported {
        value["projectId"] = json!("project-1");
    }
    value
}

fn candidates(result: &AgentSessionScanResult) -> Value {
    serde_json::to_value(&result.candidates).unwrap()
}

// ---------------------------------------------------------------------------------------------
// scan

#[tokio::test]
async fn reads_claude_project_cwds_newest_first() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, older) = mkdir("workspace-older-");
    let (_d, newer) = mkdir("workspace-newer-");
    // Slugs are lossy on purpose: the scanner must not decode them.
    write_transcript(
        &claude.join("projects/-slug-older/a.jsonl"),
        &claude_session_line(&older),
        ms("2026-01-01T00:00:00.000Z"),
    );
    write_transcript(
        &claude.join("projects/-slug-older/b.jsonl"),
        &claude_session_line(&older),
        ms("2026-01-02T00:00:00.000Z"),
    );
    write_transcript(
        &claude.join("projects/-slug-newer/c.jsonl"),
        &claude_session_line(&newer),
        ms("2026-03-01T00:00:00.000Z"),
    );
    let result = run_scan(Input::new(&claude, &codex)).await;
    assert_eq!(
        candidates(&result),
        json!([
            candidate(&newer, &["claudeAgent"], 1, "2026-03-01T00:00:00.000Z", false, Value::Null),
            candidate(&older, &["claudeAgent"], 2, "2026-01-02T00:00:00.000Z", false, Value::Null),
        ])
    );
    assert_eq!(result.truncated, None);
}

#[tokio::test]
async fn groups_codex_rollouts_by_cwd_across_date_directories() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let (_d, other) = mkdir("workspace-other-");
    write_transcript(
        &rollout(&codex, "2026-01-05", "rollout-2026-01-05T10-00-00-aaa.jsonl"),
        &codex_rollout_line(&workspace),
        ms("2026-01-05T10:00:00.000Z"),
    );
    write_transcript(
        &rollout(&codex, "2026-02-09", "rollout-2026-02-09T10-00-00-bbb.jsonl"),
        &codex_rollout_line(&workspace),
        ms("2026-02-09T10:00:00.000Z"),
    );
    write_transcript(
        &rollout(&codex, "2026-02-09", "rollout-2026-02-09T11-00-00-ccc.jsonl"),
        &codex_rollout_line(&other),
        ms("2026-02-09T11:00:00.000Z"),
    );
    let result = run_scan(Input::new(&claude, &codex)).await;
    assert_eq!(
        candidates(&result),
        json!([
            candidate(&other, &["codex"], 1, "2026-02-09T11:00:00.000Z", false, Value::Null),
            candidate(&workspace, &["codex"], 2, "2026-02-09T10:00:00.000Z", false, Value::Null),
        ])
    );
}

#[tokio::test]
async fn does_not_open_a_non_file_transcript() {
    for source in [AgentSessionSource::ClaudeAgent, AgentSessionSource::Codex] {
        let (_a, claude) = mkdir("claude-home-");
        let (_b, codex) = mkdir("codex-home-");
        let transcript = if source == AgentSessionSource::ClaudeAgent {
            claude.join("projects/-slug/session.jsonl")
        } else {
            rollout(&codex, "2026-08-24", "rollout-session.jsonl")
        };
        std::fs::create_dir_all(&transcript).unwrap();
        let opens = Arc::new(AtomicUsize::new(0));
        let mut input = Input::new(&claude, &codex);
        let (counter, target) = (opens.clone(), transcript.clone());
        input.fs.open = Some(Arc::new(move |path| {
            if path == target {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            None
        }));
        let result = run_scan(input).await;
        assert!(result.candidates.is_empty());
        assert_eq!(opens.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn stops_directory_reads_at_the_discovery_operation_budget() {
    for source in [AgentSessionSource::ClaudeAgent, AgentSessionSource::Codex] {
        let (_a, claude) = mkdir("claude-home-");
        let (_b, codex) = mkdir("codex-home-");
        let root = if source == AgentSessionSource::ClaudeAgent {
            claude.join("projects")
        } else {
            codex.join("sessions")
        };
        let empty: Vec<String> = (0..20_001).map(|index| format!("empty-{index:05}")).collect();
        let reads = Arc::new(AtomicUsize::new(0));
        let mut input = Input::new(&claude, &codex);
        let counter = reads.clone();
        input.fs.dir = Some(Arc::new(move |directory| {
            if directory == root {
                counter.fetch_add(1, Ordering::SeqCst);
                return Some(Ok(empty.clone()));
            }
            if directory.parent() == Some(root.as_path()) {
                counter.fetch_add(1, Ordering::SeqCst);
                return Some(Ok(Vec::new()));
            }
            None
        }));
        let result = run_scan(input).await;
        assert!(result.candidates.is_empty());
        assert_eq!(reads.load(Ordering::SeqCst), 20_000, "{source:?}");
    }
}

#[tokio::test]
async fn merges_the_same_cwd_seen_by_both_agents_and_flags_imported_projects() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&workspace),
        ms("2026-01-01T00:00:00.000Z"),
    );
    write_transcript(
        &rollout(&codex, "2026-04-01", "rollout-2026-04-01T09-00-00-aaa.jsonl"),
        &codex_rollout_line(&workspace),
        ms("2026-04-01T09:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.imported = vec![workspace.clone()];
    let result = run_scan(input).await;
    assert_eq!(
        candidates(&result),
        json!([candidate(
            &workspace,
            &["claudeAgent", "codex"],
            2,
            "2026-04-01T09:00:00.000Z",
            true,
            Value::Null
        )])
    );
}

#[tokio::test]
async fn returns_the_imported_project_id_through_a_realpath_alias() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let (_d, links) = mkdir("scanner-links-");
    let alias = links.join("workspace-alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&alias),
        ms("2026-01-01T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.imported = vec![workspace.clone()];
    let result = run_scan(input).await;
    let first = &result.candidates[0];
    assert_eq!(first.path, s(&workspace));
    assert_eq!(first.project_id.as_ref().map(|id| id.as_str()), Some("project-1"));
    assert!(first.already_imported);
    assert_eq!(first.git, Some(None));
}

#[tokio::test]
async fn matches_a_persisted_project_alias_to_a_transcript_realpath() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let (_d, links) = mkdir("scanner-links-");
    let alias = links.join("workspace-alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&workspace),
        ms("2026-01-01T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.imported = vec![alias.clone()];
    let result = run_scan(input).await;
    let first = &result.candidates[0];
    assert_eq!(first.path, s(&alias));
    assert_eq!(first.project_id.as_ref().map(|id| id.as_str()), Some("project-1"));
    assert!(first.already_imported);
}

fn alias_stat(alias: PathBuf, target: PathBuf) -> StatHook {
    Arc::new(move |path| (path == alias).then(|| RealFileSystem.stat(&target)))
}

#[tokio::test]
async fn merges_case_aliases_and_preserves_the_persisted_project_path() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let alias = workspace.parent().unwrap().join(basename(&workspace).to_uppercase());
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&alias),
        ms("2026-01-01T00:00:00.000Z"),
    );
    write_transcript(
        &rollout(&codex, "2026-01-02", "rollout-b.jsonl"),
        &codex_rollout_line(&workspace),
        ms("2026-01-02T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.imported = vec![workspace.clone()];
    input.fs.stat = Some(alias_stat(alias, workspace.clone()));
    let result = run_scan(input).await;
    assert_eq!(
        candidates(&result),
        json!([candidate(
            &workspace,
            &["claudeAgent", "codex"],
            2,
            "2026-01-02T00:00:00.000Z",
            true,
            Value::Null
        )])
    );
}

#[tokio::test]
async fn keeps_case_variants_distinct_when_the_filesystem_identities_differ() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, backing_upper) = mkdir("backing-upper-");
    let (_d, backing_lower) = mkdir("backing-lower-");
    let (_e, parent) = mkdir("case-aliases-");
    let upper = parent.join("Repo");
    let lower = parent.join("repo");
    write_transcript(
        &claude.join("projects/-upper/a.jsonl"),
        &claude_session_line(&upper),
        ms("2026-01-02T00:00:00.000Z"),
    );
    write_transcript(
        &claude.join("projects/-lower/b.jsonl"),
        &claude_session_line(&lower),
        ms("2026-01-01T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    let (u, l) = (upper.clone(), lower.clone());
    input.fs.stat = Some(Arc::new(move |path| {
        if path == u {
            Some(RealFileSystem.stat(&backing_upper))
        } else if path == l {
            Some(RealFileSystem.stat(&backing_lower))
        } else {
            None
        }
    }));
    let result = run_scan(input).await;
    assert_eq!(paths(&result), [s(&upper), s(&lower)]);
}

#[tokio::test]
async fn uses_explicit_provider_instance_homes_instead_of_overridden_legacy_homes() {
    let (_a, claude) = mkdir("claude-legacy-");
    let (_b, codex) = mkdir("codex-legacy-");
    let (_c, claude_instance) = mkdir("claude-instance-");
    let (_d, codex_instance) = mkdir("codex-instance-");
    let (_e, legacy_workspace) = mkdir("workspace-legacy-");
    let (_f, claude_workspace) = mkdir("workspace-claude-");
    let (_g, codex_workspace) = mkdir("workspace-codex-");
    write_transcript(
        &claude.join("projects/-legacy/session.jsonl"),
        &claude_session_line(&legacy_workspace),
        ms("2026-01-01T00:00:00.000Z"),
    );
    write_transcript(
        &claude_instance.join("projects/-actual/session.jsonl"),
        &claude_session_line(&claude_workspace),
        ms("2026-02-01T00:00:00.000Z"),
    );
    write_transcript(
        &rollout(&codex_instance, "2026-03-01", "rollout-instance.jsonl"),
        &codex_rollout_line(&codex_workspace),
        ms("2026-03-01T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({
        "claudeAgent": {"driver": "claudeAgent", "config": {"homePath": claude_instance}},
        "codex": {"driver": "codex", "config": {"homePath": codex_instance}},
    });
    assert_eq!(paths(&run_scan(input).await), [s(&codex_workspace), s(&claude_workspace)]);
}

#[tokio::test]
async fn scans_each_distinct_home_across_multiple_instances_once() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, other_codex) = mkdir("codex-other-");
    let (_d, workspace) = mkdir("workspace-");
    let (_e, other_workspace) = mkdir("workspace-other-");
    for (home, cwd) in [(&codex, &workspace), (&other_codex, &other_workspace)] {
        write_transcript(
            &rollout(home, "2026-01-01", "rollout-session.jsonl"),
            &codex_rollout_line(cwd),
            ms("2026-01-01T00:00:00.000Z"),
        );
    }
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({
        "codex-personal": {"driver": "codex", "config": {"homePath": codex}},
        "codex-work": {"driver": "codex", "config": {"homePath": other_codex}},
    });
    let result = run_scan(input).await;
    assert_eq!(result.candidates.iter().map(|c| c.thread_count).collect::<Vec<_>>(), [1, 1]);
    let mut found = paths(&result);
    found.sort();
    let mut expected = vec![s(&workspace), s(&other_workspace)];
    expected.sort();
    assert_eq!(found, expected);
}

#[tokio::test]
async fn honors_provider_instance_home_directory_environment_variables() {
    let (_a, claude) = mkdir("claude-legacy-");
    let (_b, codex) = mkdir("codex-legacy-");
    let (_c, claude_env) = mkdir("claude-env-");
    let (_d, codex_env) = mkdir("codex-env-");
    let (_e, claude_workspace) = mkdir("workspace-claude-");
    let (_f, codex_workspace) = mkdir("workspace-codex-");
    write_transcript(
        &claude_env.join("projects/-actual/session.jsonl"),
        &claude_session_line(&claude_workspace),
        ms("2026-01-01T00:00:00.000Z"),
    );
    write_transcript(
        &rollout(&codex_env, "2026-01-01", "rollout-session.jsonl"),
        &codex_rollout_line(&codex_workspace),
        ms("2026-01-02T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({
        "claudeAgent": {"driver": "claudeAgent", "environment": [{"name": "CLAUDE_CONFIG_DIR", "value": claude_env, "sensitive": false}], "config": {}},
        "codex": {"driver": "codex", "environment": [{"name": "CODEX_HOME", "value": codex_env, "sensitive": false}], "config": {}},
    });
    assert_eq!(paths(&run_scan(input).await), [s(&codex_workspace), s(&claude_workspace)]);
}

#[tokio::test]
async fn ignores_invalid_provider_instances_while_scanning_the_remaining_providers() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    write_transcript(
        &claude.join("projects/-actual/session.jsonl"),
        &claude_session_line(&workspace),
        ms("2026-01-01T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({"codex": {"driver": "codex", "config": {"homePath": 123}}});
    assert_eq!(paths(&run_scan(input).await), [s(&workspace)]);
}

#[tokio::test]
async fn does_not_scan_provider_instances_disabled_by_the_envelope_or_config() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, envelope_home) = mkdir("codex-disabled-envelope-");
    let (_d, config_home) = mkdir("codex-disabled-config-");
    let (_e, envelope_workspace) = mkdir("workspace-disabled-envelope-");
    let (_f, config_workspace) = mkdir("workspace-disabled-config-");
    for (home, workspace, session) in [
        (&envelope_home, &envelope_workspace, "envelope-disabled"),
        (&config_home, &config_workspace, "config-disabled"),
    ] {
        write_transcript(
            &rollout(home, "2026-08-24", &format!("rollout-{session}.jsonl")),
            &codex_rollout_line(workspace),
            ms("2026-08-24T12:00:00.000Z"),
        );
    }
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({
        "codex-envelope-disabled": {"driver": "codex", "enabled": false, "config": {"homePath": envelope_home}},
        "codex-config-disabled": {"driver": "codex", "config": {"enabled": false, "homePath": config_home}},
    });
    assert!(run_scan(input).await.candidates.is_empty());
}

#[tokio::test]
async fn ignores_relative_working_directories_from_malformed_transcripts() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    write_transcript(
        &claude.join("projects/-relative/session.jsonl"),
        &claude_session_line(Path::new("some/relative/workspace")),
        ms("2026-01-01T00:00:00.000Z"),
    );
    assert!(run_scan(Input::new(&claude, &codex)).await.candidates.is_empty());
}

#[tokio::test]
async fn drops_candidates_whose_directory_no_longer_exists() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&claude.join("does-not-exist")),
        ms("2026-01-01T00:00:00.000Z"),
    );
    assert!(run_scan(Input::new(&claude, &codex)).await.candidates.is_empty());
}

#[tokio::test]
async fn excludes_the_home_directory_temporary_root_and_data_directory() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, base) = mkdir("scanner-base-");
    let (_d, home) = mkdir("scanner-user-home-");
    let (_e, workspace) = mkdir("workspace-");
    let tmp = std::env::temp_dir();
    for (index, cwd) in [home.clone(), tmp, base.clone(), workspace.clone()].iter().enumerate() {
        write_transcript(
            &claude.join(format!("projects/-slug-{index}/session.jsonl")),
            &claude_session_line(cwd),
            ms("2026-01-01T00:00:00.000Z") + index as i64,
        );
    }
    let mut input = Input::new(&claude, &codex);
    input.base_dir = Some(base);
    input.home = Some(home);
    assert_eq!(paths(&run_scan(input).await), [s(&workspace)]);
}

#[tokio::test]
async fn excludes_managed_worktree_sandboxes() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let worktree = claude.join(".t3/worktrees/sample-app/wt-1");
    std::fs::create_dir_all(&worktree).unwrap();
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&worktree),
        ms("2026-01-01T00:00:00.000Z"),
    );
    assert!(run_scan(Input::new(&claude, &codex)).await.candidates.is_empty());
}

#[tokio::test]
async fn excludes_codex_scratch_directories_and_downloads() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, home) = mkdir("scanner-user-home-");
    let (_d, keep) = mkdir("workspace-keep-");
    let scratch = home.join("Documents/Codex/run/2026-09-01/some-conversation");
    let downloads = home.join("Downloads/run");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::create_dir_all(&downloads).unwrap();
    for (index, cwd) in [&scratch, &downloads, &keep].iter().enumerate() {
        write_transcript(
            &rollout(&codex, "2026-09-01", &format!("rollout-{index}.jsonl")),
            &codex_rollout_line(cwd),
            ms("2026-09-01T00:00:00.000Z"),
        );
    }
    let mut input = Input::new(&claude, &codex);
    input.home = Some(home);
    assert_eq!(paths(&run_scan(input).await), [s(&keep)]);
}

#[tokio::test]
async fn skips_linked_git_worktrees_and_reports_the_origin_of_real_checkouts() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, repo) = mkdir("workspace-repo-");
    let (_d, worktree) = mkdir("workspace-worktree-");
    let (_e, plain) = mkdir("workspace-plain-");
    let (_f, no_remote) = mkdir("workspace-noremote-");
    let (_g, submodule) = mkdir("workspace-submodule-");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(
        repo.join(".git/config"),
        "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:octo-org/sample-app.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
    )
    .unwrap();
    std::fs::write(worktree.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display())).unwrap();
    std::fs::create_dir_all(no_remote.join(".git")).unwrap();
    std::fs::write(no_remote.join(".git/config"), "[core]\n").unwrap();
    // Submodules use a gitdir pointer too, but into `modules/`.
    let module_dir = repo.join(".git/modules/vendor");
    std::fs::create_dir_all(&module_dir).unwrap();
    std::fs::write(module_dir.join("config"), "[remote \"origin\"]\n\turl = ssh://github.com/octo-org/vendor.git\n").unwrap();
    std::fs::write(submodule.join(".git"), format!("gitdir: {}\n", module_dir.display())).unwrap();
    for (index, cwd) in [&repo, &worktree, &plain, &no_remote, &submodule].iter().enumerate() {
        write_transcript(
            &claude.join(format!("projects/-slug-{index}/a.jsonl")),
            &claude_session_line(cwd),
            ms(&format!("2026-01-0{}T00:00:00.000Z", index + 1)),
        );
    }
    let result = run_scan(Input::new(&claude, &codex)).await;
    let summary: Vec<Value> = result.candidates.iter().map(|c| json!({"path": c.path, "git": c.git})).collect();
    assert_eq!(
        Value::Array(summary),
        json!([
            {"path": submodule, "git": {"remoteKey": "github.com/octo-org/vendor", "repository": "octo-org/vendor"}},
            {"path": no_remote, "git": {"remoteKey": null, "repository": null}},
            {"path": plain, "git": null},
            {"path": repo, "git": {"remoteKey": "github.com/octo-org/sample-app", "repository": "octo-org/sample-app"}},
        ])
    );
}

#[tokio::test]
async fn excludes_sandboxes_under_the_configured_worktrees_dir() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, base) = mkdir("scanner-base-");
    let worktree = base.join("worktrees/sample-app/wt-2");
    std::fs::create_dir_all(&worktree).unwrap();
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&worktree),
        ms("2026-01-01T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.base_dir = Some(base);
    assert!(run_scan(input).await.candidates.is_empty());
}

#[tokio::test]
async fn excludes_sandboxes_reached_through_a_symlink_into_the_worktrees_dir() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    // Canonical, so the link's realpath shares the worktrees directory's spelling (macOS temp
    // directories live behind the `/var` → `/private/var` link).
    let (_c, base) = temp_dir("scanner-base-");
    let (_d, links) = mkdir("scanner-links-");
    let worktree = base.join("worktrees/sample-app/wt-3");
    std::fs::create_dir_all(&worktree).unwrap();
    let innocent = links.join("innocent-project");
    std::os::unix::fs::symlink(&worktree, &innocent).unwrap();
    write_transcript(
        &claude.join("projects/-slug/a.jsonl"),
        &claude_session_line(&innocent),
        ms("2026-01-01T00:00:00.000Z"),
    );
    let mut input = Input::new(&claude, &codex);
    input.base_dir = Some(base);
    assert!(run_scan(input).await.candidates.is_empty());
}

#[tokio::test]
async fn finds_the_cwd_on_a_later_line_when_the_first_records_carry_none() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let contents = format!(
        "{{\"type\":\"file-history-snapshot\",\"messageId\":\"m1\"}}\n{{\"type\":\"queue-operation\",\"operation\":\"enqueue\"}}\n{}",
        claude_session_line(&workspace)
    );
    write_transcript(&claude.join("projects/-slug/a.jsonl"), &contents, ms("2026-01-01T00:00:00.000Z"));
    assert_eq!(paths(&run_scan(Input::new(&claude, &codex)).await), [s(&workspace)]);
}

#[tokio::test]
async fn reads_a_complete_record_at_the_exact_chunk_boundary() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let line = claude_session_line(&workspace).split('\n').next().unwrap().to_owned();
    let prefix = "{\"padding\":\"";
    let suffix = format!("\",{}", &line[1..]);
    let contents = format!("{prefix}{}{suffix}", "x".repeat(32 * 1024 - prefix.len() - suffix.len()));
    assert_eq!(contents.len(), 32 * 1024);
    write_transcript(&claude.join("projects/-exact/session.jsonl"), &contents, ms("2026-01-01T00:00:00.000Z"));
    assert_eq!(paths(&run_scan(Input::new(&claude, &codex)).await), [s(&workspace)]);
}

#[tokio::test]
async fn finds_session_metadata_after_a_first_record_larger_than_one_chunk() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let history = format!("{{\"type\":\"file-history-snapshot\",\"data\":\"{}\"}}\n", "x".repeat(32 * 1024));
    write_transcript(
        &claude.join("projects/-large/session.jsonl"),
        &format!("{history}{}", claude_session_line(&workspace)),
        ms("2026-01-01T00:00:00.000Z"),
    );
    assert_eq!(paths(&run_scan(Input::new(&claude, &codex)).await), [s(&workspace)]);
}

#[tokio::test]
async fn shares_metadata_bytes_across_homes_for_one_mib_files() {
    for count in [64usize, 65] {
        let (_a, claude) = mkdir("metadata-home-");
        let (_b, second) = mkdir("metadata-second-");
        let (_c, codex) = mkdir("metadata-codex-");
        let (_d, first_workspace) = mkdir("metadata-first-project-");
        let (_e, second_workspace) = mkdir("metadata-second-project-");
        let directories = [claude.join("projects/p"), second.join("projects/p")];
        let templates: Vec<PathBuf> = directories.iter().map(|d| d.join("template.jsonl")).collect();
        for (index, workspace) in [&first_workspace, &second_workspace].iter().enumerate() {
            let line = record(json!({"cwd": workspace}));
            write_transcript(
                &templates[index],
                &format!("{}{line}", " ".repeat(1024 * 1024 - line.len())),
                ms("2026-01-01T00:00:00.000Z") - index as i64 * 1000,
            );
        }
        let reserved = Arc::new(AtomicUsize::new(0));
        let opens = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut input = Input::new(&claude, &codex);
        input.provider_instances = json!({"claude-work": {"driver": "claudeAgent", "config": {"homePath": second}}});
        let dirs = directories.clone();
        input.fs.dir = Some(Arc::new(move |directory| {
            let index = dirs.iter().position(|d| d == directory)?;
            let length = if index == 0 { 32 } else { count - 32 };
            Some(Ok((0..length).map(|item| format!("session-{item}.jsonl")).collect()))
        }));
        let (dirs, temps) = (directories.clone(), templates.clone());
        let resolve = Arc::new(move |path: &Path| -> PathBuf {
            match dirs.iter().position(|d| Some(d.as_path()) == path.parent()) {
                Some(index) => temps[index].clone(),
                None => path.to_path_buf(),
            }
        });
        let r = resolve.clone();
        input.fs.stat = Some(Arc::new(move |path| Some(RealFileSystem.stat(&r(path)))));
        let (dirs, r, o, res, req) = (directories.clone(), resolve.clone(), opens.clone(), reserved.clone(), requests.clone());
        input.fs.open = Some(Arc::new(move |path| {
            if !dirs.iter().any(|d| Some(d.as_path()) == path.parent()) {
                return None;
            }
            o.fetch_add(1, Ordering::SeqCst);
            let (res, req) = (res.clone(), req.clone());
            Some(RealFileSystem.open(&r(path)).map(|inner| {
                Box::new(ObservedFile {
                    inner,
                    on_read: Arc::new(move |size, _| {
                        res.fetch_add(size, Ordering::SeqCst);
                        req.lock().unwrap().push(size);
                    }),
                }) as Box<dyn ScanFile>
            }))
        }));
        let result = run_scan(input).await;
        assert_eq!(paths(&result), [s(&first_workspace), s(&second_workspace)]);
        assert_eq!(result.candidates.iter().map(|c| c.thread_count).collect::<Vec<_>>(), [32, 32]);
        assert_eq!(result.truncated, (count == 65).then_some(true));
        assert_eq!(opens.load(Ordering::SeqCst), 64);
        assert_eq!(reserved.load(Ordering::SeqCst), 64 * 1024 * 1024);
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0], 8 * 1024);
        assert_eq!(*requests.iter().max().unwrap(), 8 * 1024);
    }
}

#[tokio::test]
async fn bounds_metadata_open_and_read_calls_for_short_read_files() {
    for count in [50usize, 51] {
        let (_a, claude) = mkdir("short-metadata-home-");
        let (_b, codex) = mkdir("short-metadata-codex-");
        let (_c, workspace) = mkdir("short-metadata-project-");
        let directory = claude.join("projects/p");
        let template = directory.join("template.jsonl");
        let line = record(json!({"cwd": workspace}));
        let contents = format!("{}{line}", " ".repeat(399 - line.len()));
        write_transcript(&template, &contents, ms("2026-01-01T00:00:00.000Z"));
        let operations = Arc::new(AtomicUsize::new(0));
        let mut input = Input::new(&claude, &codex);
        let d = directory.clone();
        input.fs.dir = Some(Arc::new(move |target| {
            (target == d).then(|| Ok((0..count).map(|i| format!("session-{i}.jsonl")).collect()))
        }));
        let (d, t) = (directory.clone(), template.clone());
        input.fs.stat = Some(Arc::new(move |path| (path.parent() == Some(d.as_path())).then(|| RealFileSystem.stat(&t))));
        let (d, t, ops) = (directory.clone(), template.clone(), operations.clone());
        let bytes = contents.into_bytes();
        input.fs.open = Some(Arc::new(move |path| {
            if path.parent() != Some(d.as_path()) {
                return None;
            }
            ops.fetch_add(1, Ordering::SeqCst);
            Some(Ok(Box::new(ByteByByte {
                bytes: bytes.clone(),
                offset: 0,
                reads: ops.clone(),
                stat: RealFileSystem.stat(&t).unwrap(),
            }) as Box<dyn ScanFile>))
        }));
        let result = run_scan(input).await;
        assert_eq!(operations.load(Ordering::SeqCst), 20_000);
        assert_eq!(result.candidates[0].thread_count, 50);
        assert_eq!(result.truncated, (count == 51).then_some(true));
    }
}

#[tokio::test]
async fn bounds_malformed_metadata_records_without_excluding_another_account() {
    let (_a, claude) = mkdir("record-metadata-home-");
    let (_b, second) = mkdir("record-metadata-second-");
    let (_c, codex) = mkdir("record-metadata-codex-");
    let (_d, workspace) = mkdir("record-metadata-project-");
    let directory = claude.join("projects/p");
    let template = directory.join("template.jsonl");
    write_transcript(&template, &"x\n".repeat(1_001), ms("2026-01-02T00:00:00.000Z"));
    write_transcript(
        &second.join("projects/p/session.jsonl"),
        &record(json!({"cwd": workspace})),
        ms("2026-01-01T00:00:00.000Z"),
    );
    let opens = Arc::new(AtomicUsize::new(0));
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({"claude-work": {"driver": "claudeAgent", "config": {"homePath": second}}});
    let d = directory.clone();
    input.fs.dir = Some(Arc::new(move |target| {
        (target == d).then(|| Ok((0..102).map(|i| format!("session-{i}.jsonl")).collect()))
    }));
    let (d, t) = (directory.clone(), template.clone());
    input.fs.stat = Some(Arc::new(move |path| (path.parent() == Some(d.as_path())).then(|| RealFileSystem.stat(&t))));
    let (d, t, o) = (directory.clone(), template.clone(), opens.clone());
    input.fs.open = Some(Arc::new(move |path| {
        if path.parent() != Some(d.as_path()) {
            return None;
        }
        o.fetch_add(1, Ordering::SeqCst);
        Some(RealFileSystem.open(&t))
    }));
    let result = run_scan(input).await;
    assert_eq!(paths(&result), [s(&workspace)]);
    assert_eq!(opens.load(Ordering::SeqCst), 100);
    assert_eq!(result.truncated, Some(true));
}

#[tokio::test]
async fn reports_unfinished_directory_work() {
    for count in [19_999usize, 20_000] {
        let (_a, claude) = mkdir("directory-budget-home-");
        let (_b, codex) = mkdir("directory-budget-codex-");
        let projects = claude.join("projects");
        let reads = Arc::new(AtomicUsize::new(0));
        let mut input = Input::new(&claude, &codex);
        let (p, r) = (projects.clone(), reads.clone());
        input.fs.dir = Some(Arc::new(move |directory| {
            if directory == p {
                r.fetch_add(1, Ordering::SeqCst);
                return Some(Ok((0..count).map(|i| format!("project-{i}")).collect()));
            }
            if directory.parent() == Some(p.as_path()) {
                r.fetch_add(1, Ordering::SeqCst);
                return Some(Ok(Vec::new()));
            }
            None
        }));
        let result = run_scan(input).await;
        assert_eq!(reads.load(Ordering::SeqCst), 20_000);
        assert!(result.candidates.is_empty());
        assert_eq!(result.truncated, (count == 20_000).then_some(true));
    }
}

#[tokio::test]
async fn skips_malformed_transcripts_without_failing_the_scan() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    write_transcript(&claude.join("projects/-broken/a.jsonl"), "not json at all\n", ms("2026-05-01T00:00:00.000Z"));
    write_transcript(
        &claude.join("projects/-no-cwd/a.jsonl"),
        "{\"type\":\"summary\"}\n",
        ms("2026-05-02T00:00:00.000Z"),
    );
    write_transcript(
        &claude.join("projects/-good/a.jsonl"),
        &claude_session_line(&workspace),
        ms("2026-05-03T00:00:00.000Z"),
    );
    let result = run_scan(Input::new(&claude, &codex)).await;
    assert_eq!(
        candidates(&result),
        json!([candidate(&workspace, &["claudeAgent"], 1, "2026-05-03T00:00:00.000Z", false, Value::Null)])
    );
}

#[tokio::test]
async fn returns_an_empty_result_when_neither_home_exists() {
    let (_a, root) = mkdir("missing-homes-");
    let result = run_scan(Input::new(&root.join("no-claude"), &root.join("no-codex"))).await;
    assert!(result.candidates.is_empty());
    assert_eq!(result.scanned_at, "2026-08-24T12:00:00.000Z");
}

// ---------------------------------------------------------------------------------------------
// recentThreads

fn record_limit_transcript(cwd: &Path, overflow: bool) -> String {
    let mut records = [
        record(json!({"type": "session_meta", "payload": {"id": "record-limit-session", "cwd": cwd}})),
        record(json!({"type": "event_msg", "payload": {"type": "user_message", "message": "First prompt"}})),
    ]
    .join("\n");
    records.push('\n');
    records.push_str(&"{}\n".repeat(99_998));
    if overflow {
        records.push('\n');
        records.push_str(&record(
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": "Overflow prompt"}}),
        ));
        records.push('\n');
    }
    records
}

#[tokio::test]
async fn counts_terminal_newlines_correctly_with_record_overflow() {
    for overflow in [false, true] {
        let (_a, claude) = mkdir("record-limit-claude-");
        let (_b, codex) = mkdir("record-limit-codex-");
        let (_c, workspace) = mkdir("record-limit-project-");
        write_transcript(
            &rollout(&codex, "2026-08-24", "rollout-records.jsonl"),
            &record_limit_transcript(&workspace, overflow),
            NOW_MS,
        );
        write_transcript(
            &rollout(&codex, "2026-08-24", "rollout-older.jsonl"),
            &codex_session("older-session", &workspace, "Older prompt"),
            NOW_MS - 1000,
        );
        let outcomes = run_outcomes(Input::new(&claude, &codex), &workspace).await;
        assert_eq!(tags(&outcomes), if overflow { ["Skipped", "Importable"] } else { ["Importable", "Skipped"] });
        let texts: Vec<String> = importable(&outcomes).iter().flat_map(|t| t.messages.iter().map(|m| m.text.clone())).collect();
        assert_eq!(texts, [if overflow { "Older prompt" } else { "First prompt" }]);
    }
}

#[tokio::test]
async fn imports_recent_claude_and_codex_sessions_for_the_selected_project_only() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let (_d, other) = mkdir("workspace-other-");
    let claude_transcript = |cwd: &Path, session: &str| {
        format!(
            "{}\n{}\n",
            record(
                json!({"type": "user", "cwd": cwd, "sessionId": session, "timestamp": "2026-08-23T12:00:00.000Z", "message": {"role": "user", "content": "Fix the project"}})
            ),
            record(
                json!({"type": "assistant", "sessionId": session, "timestamp": "2026-08-23T12:01:00.000Z", "message": {"role": "assistant", "content": [{"type": "text", "text": "Done"}]}})
            ),
        )
    };
    write_transcript(
        &claude.join("projects/-selected/claude-recent.jsonl"),
        &claude_transcript(&workspace, "claude-recent"),
        NOW_MS - DAY_MS,
    );
    write_transcript(
        &claude.join("projects/-selected/claude-old.jsonl"),
        &claude_transcript(&workspace, "claude-old"),
        NOW_MS - 31 * DAY_MS,
    );
    write_transcript(
        &claude.join("projects/-other/claude-other.jsonl"),
        &claude_transcript(&other, "claude-other"),
        NOW_MS - DAY_MS,
    );
    let codex_contents = [
        record(json!({"type": "session_meta", "payload": {"id": "codex-recent", "cwd": workspace}})),
        record(json!({"type": "event_msg", "timestamp": "2026-08-24T10:00:00.000Z", "payload": {"type": "user_message", "message": "Review this code"}})),
        record(json!({"type": "response_item", "timestamp": "2026-08-24T10:01:00.000Z", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Looks good"}]}})),
    ]
    .join("\n");
    write_transcript(
        &rollout(&codex, "2026-08-24", "rollout-codex-recent.jsonl"),
        &codex_contents,
        NOW_MS - 60 * 60 * 1000,
    );
    let threads = importable(&run_outcomes(Input::new(&claude, &codex), &workspace).await);
    assert_eq!(
        threads.iter().map(|t| t.provider_session_id.as_str()).collect::<Vec<_>>(),
        ["codex-recent", "claude-recent"]
    );
    let texts: Vec<Vec<&str>> = threads.iter().map(|t| t.messages.iter().map(|m| m.text.as_str()).collect()).collect();
    assert_eq!(texts, vec![vec!["Review this code", "Looks good"], vec!["Fix the project", "Done"]]);
}

#[tokio::test]
async fn imports_history_recorded_with_a_case_alias() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let alias = workspace.parent().unwrap().join(basename(&workspace).to_uppercase());
    let contents = [
        record(json!({"type": "user", "cwd": alias, "sessionId": "case-session", "timestamp": "2026-08-24T10:00:00.000Z", "message": {"role": "user", "content": "Import case alias history"}})),
        record(json!({"type": "assistant", "sessionId": "case-session", "timestamp": "2026-08-24T10:01:00.000Z", "message": {"role": "assistant", "content": "Imported"}})),
    ]
    .join("\n");
    write_transcript(&claude.join("projects/-alias/case-session.jsonl"), &contents, NOW_MS);
    let mut input = Input::new(&claude, &codex);
    input.fs.stat = Some(alias_stat(alias, workspace.clone()));
    let threads = importable(&run_outcomes(input, &workspace).await);
    assert_eq!(threads.iter().map(|t| t.provider_session_id.as_str()).collect::<Vec<_>>(), ["case-session"]);
}

#[tokio::test]
async fn keeps_the_provider_instance_that_owns_a_custom_session_home() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, custom) = mkdir("codex-custom-");
    let (_d, workspace) = mkdir("workspace-");
    write_transcript(
        &rollout(&custom, "2026-08-24", "rollout-custom.jsonl"),
        &codex_session("custom-session", &workspace, "Use my work account"),
        NOW_MS,
    );
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({"codex-work": {"driver": "codex", "config": {"homePath": custom}}});
    let threads = importable(&run_outcomes(input, &workspace).await);
    assert_eq!(threads[0].provider_instance_id.as_str(), "codex-work");
}

#[tokio::test]
async fn suppresses_duplicate_session_copies_without_reporting_a_skipped_import() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let contents = codex_session("copied-session", &workspace, "Import this session once");
    write_transcript(&rollout(&codex, "2026-08-24", "rollout-copy-a.jsonl"), &contents, NOW_MS);
    write_transcript(&rollout(&codex, "2026-08-24", "rollout-copy-b.jsonl"), &contents, NOW_MS - 1);
    let outcomes = run_outcomes(Input::new(&claude, &codex), &workspace).await;
    assert_eq!(tags(&outcomes), ["Importable", "Duplicate"]);
    assert_eq!(importable(&outcomes)[0].provider_session_id, "copied-session");
}

#[tokio::test]
async fn streams_large_transcripts_across_providers_without_hiding_projects() {
    let (_a, claude) = mkdir("budget-claude-");
    let (_b, codex) = mkdir("budget-codex-");
    let (_c, workspace) = mkdir("budget-workspace-");
    let mut transcripts = Vec::new();
    for (index, source) in ["codex", "claudeAgent", "codex", "claudeAgent", "codex"].iter().enumerate() {
        let session = format!("budget-session-{index}");
        let (path, contents) = if *source == "codex" {
            (
                rollout(&codex, "2026-08-24", &format!("rollout-{session}.jsonl")),
                codex_session(&session, &workspace, "Imported prompt"),
            )
        } else {
            (
                claude.join(format!("projects/selected/{session}.jsonl")),
                record(json!({"type": "user", "cwd": workspace, "sessionId": session, "message": {"content": "Imported prompt"}})),
            )
        };
        let mut padded = format!("{contents}\n");
        padded.push_str(&" ".repeat(16 * 1024 * 1024 - padded.len()));
        write_transcript(&path, &padded, NOW_MS - index as i64 * 1000);
        transcripts.push(path);
    }
    let opens: Arc<Mutex<HashMap<PathBuf, usize>>> = Arc::default();
    let full_read = Arc::new(AtomicUsize::new(0));
    let mut input = Input::new(&claude, &codex);
    let (o, f, tracked) = (opens.clone(), full_read.clone(), transcripts.clone());
    input.fs.open = Some(Arc::new(move |path| {
        let count = {
            let mut opens = o.lock().unwrap();
            let count = opens.entry(path.to_path_buf()).or_default();
            *count += 1;
            *count
        };
        let inner = RealFileSystem.open(path);
        if !tracked.iter().any(|t| t == path) || count == 1 {
            return Some(inner);
        }
        let f = f.clone();
        Some(inner.map(|inner| {
            Box::new(ObservedFile {
                inner,
                on_read: Arc::new(move |_, chunk| {
                    if let Some(chunk) = chunk {
                        f.fetch_add(chunk.len(), Ordering::SeqCst);
                    }
                }),
            }) as Box<dyn ScanFile>
        }))
    }));
    let harness = harness(input).await;
    let scan = harness.scanner.scan().await.unwrap();
    assert_eq!(scan.candidates[0].thread_count, 5);
    let outcomes: Vec<RecentThread> = harness.scanner.recent_threads(&s(&workspace), Vec::new()).await.unwrap().collect().await;
    assert_eq!(tags(&outcomes), ["Importable"; 5]);
    assert_eq!(full_read.load(Ordering::SeqCst), 80 * 1024 * 1024);
}

#[tokio::test]
async fn skips_excessive_records_without_blocking_an_older_valid_transcript() {
    let (_a, claude) = mkdir("record-budget-claude-");
    let (_b, codex) = mkdir("record-budget-codex-");
    let (_c, workspace) = mkdir("record-budget-workspace-");
    for (session, padding, mtime) in [("excessive", "\n".repeat(100_001), NOW_MS), ("older", String::new(), NOW_MS - 1000)] {
        write_transcript(
            &rollout(&codex, "2026-08-24", &format!("rollout-{session}.jsonl")),
            &format!("{}{padding}", codex_session(session, &workspace, "Imported prompt")),
            mtime,
        );
    }
    let outcomes = run_outcomes(Input::new(&claude, &codex), &workspace).await;
    assert_eq!(tags(&outcomes), ["Skipped", "Importable"]);
    assert_eq!(importable(&outcomes)[0].provider_session_id, "older");
}

#[tokio::test]
async fn rechecks_the_snapshot_cwd_after_a_replacement() {
    for source in [AgentSessionSource::ClaudeAgent, AgentSessionSource::Codex] {
        for replacement in ["same root", "other root", "symlink alias", "other then same"] {
            let (_guard, fixture) = mkdir("replaced-cwd-");
            let workspace = fixture.join("original");
            let other = fixture.join("other");
            let alias = fixture.join("alias");
            let claude = fixture.join("claude");
            let codex = fixture.join("codex");
            std::fs::create_dir_all(&workspace).unwrap();
            std::fs::create_dir_all(&other).unwrap();
            if replacement == "symlink alias" {
                std::os::unix::fs::symlink(&workspace, &alias).unwrap();
            }
            let file = if source == AgentSessionSource::Codex {
                rollout(&codex, "2026-08-24", "rollout-replaced.jsonl")
            } else {
                claude.join("projects/p/replaced.jsonl")
            };
            let make = |cwd: &Path, text: &str, later: Option<&Path>| {
                let mut records: Vec<Value> = if source == AgentSessionSource::Codex {
                    vec![
                        json!({"type": "session_meta", "payload": {"id": "replacement-session", "cwd": cwd}}),
                        json!({"type": "event_msg", "payload": {"type": "user_message", "message": text}}),
                    ]
                } else {
                    vec![json!({"type": "user", "cwd": cwd, "sessionId": "replacement-session", "message": {"content": text}})]
                };
                if let Some(later) = later {
                    records.push(json!({"cwd": later}));
                }
                records.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")
            };
            write_transcript(&file, &make(&workspace, "Original prompt", None), NOW_MS);
            let harness = harness(Input::new(&claude, &codex)).await;
            let scan = harness.scanner.scan().await.unwrap();
            assert_eq!(paths(&scan), [s(&workspace)]);
            let replacement_cwd = match replacement {
                "symlink alias" => alias.clone(),
                "same root" => workspace.clone(),
                _ => other.clone(),
            };
            std::fs::remove_file(&file).unwrap();
            let later = (replacement == "other then same").then_some(workspace.as_path());
            write_transcript(&file, &make(&replacement_cwd, "Replacement prompt", later), NOW_MS);
            let outcomes: Vec<RecentThread> = harness.scanner.recent_threads(&s(&workspace), Vec::new()).await.unwrap().collect().await;
            if replacement == "same root" || replacement == "symlink alias" {
                assert_eq!(tags(&outcomes), ["Importable"], "{source:?} {replacement}");
                assert_eq!(importable(&outcomes)[0].messages[0].text, "Replacement prompt");
            } else {
                assert_eq!(tags(&outcomes), ["Skipped"], "{source:?} {replacement}");
            }
        }
    }
}

#[tokio::test]
async fn checks_file_identity_and_provider_before_skipping_completed_history() {
    let (_a, claude) = mkdir("completed-claude-");
    let (_b, codex) = mkdir("completed-codex-");
    let (_c, workspace) = mkdir("completed-workspace-");
    let file = rollout(&codex, "2026-08-24", "rollout-replaced.jsonl");
    write_transcript(&file, &codex_session("original-session", &workspace, "Imported prompt"), NOW_MS);
    let harness = harness(Input::new(&claude, &codex)).await;
    let root = s(&workspace);
    let collect = |completed: Vec<AgentSessionImportSource>| {
        let scanner = harness.scanner.clone();
        let root = root.clone();
        async move { scanner.recent_threads(&root, completed).await.unwrap().collect::<Vec<_>>().await }
    };
    let initial = collect(Vec::new()).await;
    let RecentThread::Importable { source, .. } = &initial[0] else {
        panic!("not importable");
    };
    let completed = collect(vec![source.clone()]).await;
    assert_eq!(tags(&completed), ["AlreadyImported"]);
    let mut wrong_provider = source.clone();
    wrong_provider.provider = AgentSessionSource::ClaudeAgent;
    assert_eq!(tags(&collect(vec![wrong_provider]).await), ["Importable"]);
    // Keep the old inode allocated while replacing the path with an equal-size file.
    let _held = std::fs::File::open(&file).unwrap();
    std::fs::remove_file(&file).unwrap();
    write_transcript(&file, &codex_session("replaced-session", &workspace, "Imported prompt"), NOW_MS);
    let replaced = collect(vec![source.clone()]).await;
    let RecentThread::Importable {
        thread,
        source: replaced_source,
    } = &replaced[0]
    else {
        panic!("not importable");
    };
    assert_eq!(thread.provider_session_id, "replaced-session");
    assert_eq!(replaced_source.size, source.size);
    assert_eq!(replaced_source.mtime_ms, source.mtime_ms);
}

#[tokio::test]
async fn imports_visible_history_from_a_transcript_with_an_oversized_tool_record() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let mut tool = record(json!({"type": "tool_result", "data": ""}));
    tool.push_str(&" ".repeat(16 * 1024 * 1024 + 1 - tool.len()));
    let transcript = format!("{}\n{tool}", codex_session("large-session", &workspace, "Import this large session"));
    write_transcript(&rollout(&codex, "2026-08-24", "rollout-large.jsonl"), &transcript, NOW_MS);
    let outcomes = run_outcomes(Input::new(&claude, &codex), &workspace).await;
    let threads = importable(&outcomes);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(threads[0].provider_session_id, "large-session");
    assert_eq!(
        threads[0].messages.iter().map(|m| (m.role.as_str(), m.text.as_str())).collect::<Vec<_>>(),
        [("user", "Import this large session")]
    );
}

#[tokio::test]
async fn reports_stat_read_and_parse_failures_as_skipped() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let missing = codex.join("missing.jsonl");
    let stat_path = rollout(&codex, "2026-08-24", "rollout-stat.jsonl");
    let read_path = rollout(&codex, "2026-08-24", "rollout-read.jsonl");
    let parse_path = rollout(&codex, "2026-08-24", "rollout-parse.jsonl");
    write_transcript(&stat_path, &codex_session("stat-session", &workspace, "Import this session"), NOW_MS);
    write_transcript(&read_path, &codex_session("read-session", &workspace, "Import this session"), NOW_MS);
    write_transcript(
        &parse_path,
        &record(json!({"type": "session_meta", "payload": {"id": "parse-session", "cwd": workspace}})),
        NOW_MS,
    );
    let stats = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let mut input = Input::new(&claude, &codex);
    let (sp, m, c) = (stat_path.clone(), missing.clone(), stats.clone());
    input.fs.stat = Some(Arc::new(move |path| {
        if path != sp {
            return None;
        }
        let count = c.fetch_add(1, Ordering::SeqCst) + 1;
        Some(RealFileSystem.stat(if count == 1 { path } else { &m }))
    }));
    let (rp, m, c) = (read_path.clone(), missing.clone(), opens.clone());
    input.fs.open = Some(Arc::new(move |path| {
        if path != rp {
            return None;
        }
        let count = c.fetch_add(1, Ordering::SeqCst) + 1;
        Some(RealFileSystem.open(if count == 1 { path } else { &m }))
    }));
    let outcomes = run_outcomes(input, &workspace).await;
    assert_eq!(tags(&outcomes), ["Skipped", "Skipped", "Skipped"]);
}

#[tokio::test]
async fn does_not_reopen_a_transcript_that_becomes_a_non_file_after_discovery() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let (_d, non_file) = mkdir("non-file-");
    let transcript = rollout(&codex, "2026-08-24", "rollout-changed.jsonl");
    write_transcript(&transcript, &codex_session("changed-session", &workspace, "Do not import this session"), NOW_MS);
    let stats = Arc::new(AtomicUsize::new(0));
    let opens = Arc::new(AtomicUsize::new(0));
    let mut input = Input::new(&claude, &codex);
    let (t, c) = (transcript.clone(), stats.clone());
    input.fs.stat = Some(Arc::new(move |path| {
        if path != t {
            return None;
        }
        let count = c.fetch_add(1, Ordering::SeqCst) + 1;
        Some(RealFileSystem.stat(if count == 1 { path } else { &non_file }))
    }));
    let (t, c) = (transcript.clone(), opens.clone());
    input.fs.open = Some(Arc::new(move |path| {
        if path == t {
            c.fetch_add(1, Ordering::SeqCst);
        }
        None
    }));
    let outcomes = run_outcomes(input, &workspace).await;
    assert_eq!(stats.load(Ordering::SeqCst), 2);
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    assert_eq!(tags(&outcomes), ["Skipped"]);
}

#[tokio::test]
async fn does_not_import_a_transcript_dated_after_the_current_time() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    write_transcript(
        &rollout(&codex, "2026-08-24", "rollout-future.jsonl"),
        &codex_session("future-session", &workspace, "Future work"),
        NOW_MS + 1000,
    );
    assert!(run_outcomes(Input::new(&claude, &codex), &workspace).await.is_empty());
}

#[tokio::test]
async fn skips_growth_during_reading_without_exceeding_the_reserved_bytes() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let transcript = rollout(&codex, "2026-08-24", "rollout-growing.jsonl");
    let contents = codex_session("growing-session", &workspace, "Do not import a changing file");
    write_transcript(&transcript, &contents, NOW_MS);
    let opens = Arc::new(AtomicUsize::new(0));
    let full_read = Arc::new(AtomicUsize::new(0));
    let grew = Arc::new(AtomicUsize::new(0));
    let mut input = Input::new(&claude, &codex);
    let (t, o, f, g, body) = (transcript.clone(), opens.clone(), full_read.clone(), grew.clone(), contents.clone());
    input.fs.open = Some(Arc::new(move |path| {
        if path != t {
            return None;
        }
        let count = o.fetch_add(1, Ordering::SeqCst) + 1;
        if count == 1 {
            return Some(RealFileSystem.open(path));
        }
        let (f, g, body, target) = (f.clone(), g.clone(), body.clone(), t.clone());
        Some(RealFileSystem.open(path).map(|inner| {
            Box::new(ObservedFile {
                inner,
                on_read: Arc::new(move |_, chunk| {
                    let Some(chunk) = chunk else {
                        return;
                    };
                    f.fetch_add(chunk.len(), Ordering::SeqCst);
                    if g.fetch_add(1, Ordering::SeqCst) == 0 {
                        std::fs::write(&target, format!("{body}\nchanged")).unwrap();
                    }
                }),
            }) as Box<dyn ScanFile>
        }))
    }));
    let outcomes = run_outcomes(input, &workspace).await;
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    assert_eq!(full_read.load(Ordering::SeqCst), contents.len());
    assert_eq!(tags(&outcomes), ["Skipped"]);
}

#[tokio::test]
async fn skips_a_transcript_that_shrinks_after_its_size_check() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let transcript = rollout(&codex, "2026-08-24", "rollout-shrinking.jsonl");
    let shrunk = codex.join("shrunk.jsonl");
    let contents = codex_session("shrinking-session", &workspace, "Do not import a changing file");
    write_transcript(&transcript, &format!("{contents}\n{}", "padding".repeat(100)), NOW_MS);
    write_transcript(&shrunk, &contents, NOW_MS);
    let opens = Arc::new(AtomicUsize::new(0));
    let mut input = Input::new(&claude, &codex);
    let (t, o) = (transcript.clone(), opens.clone());
    input.fs.open = Some(Arc::new(move |path| {
        if path != t {
            return None;
        }
        let count = o.fetch_add(1, Ordering::SeqCst) + 1;
        Some(RealFileSystem.open(if count == 1 { path } else { &shrunk }))
    }));
    let outcomes = run_outcomes(input, &workspace).await;
    assert_eq!(opens.load(Ordering::SeqCst), 2);
    assert_eq!(tags(&outcomes), ["Skipped"]);
}

#[tokio::test]
async fn does_not_read_the_second_transcript_when_the_consumer_takes_one_thread() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, workspace) = mkdir("workspace-");
    let older = rollout(&codex, "2026-08-23", "rollout-older.jsonl");
    let newer = rollout(&codex, "2026-08-24", "rollout-newer.jsonl");
    write_transcript(&older, &codex_session("older-session", &workspace, "Older prompt"), NOW_MS - 1000);
    write_transcript(&newer, &codex_session("newer-session", &workspace, "Newer prompt"), NOW_MS);
    let opens: Arc<Mutex<HashMap<PathBuf, usize>>> = Arc::default();
    let content_reads: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let mut input = Input::new(&claude, &codex);
    let (o, r, tracked) = (opens.clone(), content_reads.clone(), [older.clone(), newer.clone()]);
    input.fs.open = Some(Arc::new(move |path| {
        if tracked.iter().any(|t| t == path) {
            let mut opens = o.lock().unwrap();
            let count = opens.entry(path.to_path_buf()).or_default();
            *count += 1;
            if *count == 2 {
                r.lock().unwrap().push(path.to_path_buf());
            }
        }
        None
    }));
    let harness = harness(input).await;
    let threads: Vec<RecentThread> = harness
        .scanner
        .recent_threads(&s(&workspace), Vec::new())
        .await
        .unwrap()
        .take(1)
        .collect()
        .await;
    assert_eq!(
        importable(&threads).iter().map(|t| t.provider_session_id.as_str()).collect::<Vec<_>>(),
        ["newer-session"]
    );
    assert_eq!(*content_reads.lock().unwrap(), [newer]);
    assert_eq!(opens.lock().unwrap().get(&older), Some(&1));
}

#[tokio::test]
async fn does_not_import_sessions_from_a_managed_worktree() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, base) = mkdir("scanner-base-");
    let workspace = base.join("worktrees/sample-app/managed-worktree");
    std::fs::create_dir_all(&workspace).unwrap();
    write_transcript(
        &rollout(&codex, "2026-08-24", "rollout-managed.jsonl"),
        &codex_session("managed-session", &workspace, "Do not import this session"),
        NOW_MS,
    );
    let mut input = Input::new(&claude, &codex);
    input.base_dir = Some(base);
    assert!(importable(&run_outcomes(input, &workspace).await).is_empty());
}

#[tokio::test]
async fn uses_one_deterministic_provider_instance_for_a_shared_session_home() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, shared) = mkdir("codex-shared-");
    let (_d, workspace) = mkdir("workspace-");
    write_transcript(
        &rollout(&shared, "2026-08-24", "rollout-shared.jsonl"),
        &codex_session("shared-session", &workspace, "Use the shared session"),
        NOW_MS,
    );
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({
        "codex": {"driver": "codex", "config": {"homePath": shared}},
        "codex-personal": {"driver": "codex", "config": {"homePath": shared}},
        "codex-work": {"driver": "codex", "config": {"homePath": shared}},
    });
    let threads = importable(&run_outcomes(input, &workspace).await);
    assert_eq!(threads.iter().map(|t| t.provider_instance_id.as_str()).collect::<Vec<_>>(), ["codex"]);
}

#[tokio::test]
async fn uses_configured_order_when_custom_instances_share_a_session_home() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, shared) = mkdir("codex-shared-");
    let (_d, workspace) = mkdir("workspace-");
    write_transcript(
        &rollout(&shared, "2026-08-24", "rollout-shared.jsonl"),
        &codex_session("shared-session", &workspace, "Use the first account"),
        NOW_MS,
    );
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({
        "codex-work": {"driver": "codex", "config": {"homePath": shared}},
        "codex-personal": {"driver": "codex", "config": {"homePath": shared}},
    });
    let threads = importable(&run_outcomes(input, &workspace).await);
    assert_eq!(threads.iter().map(|t| t.provider_instance_id.as_str()).collect::<Vec<_>>(), ["codex-work"]);
}

#[tokio::test]
async fn keeps_a_second_account_when_the_first_has_5000_newer_files() {
    let (_a, claude) = mkdir("claude-home-");
    let (_b, codex) = mkdir("codex-home-");
    let (_c, old_workspace) = mkdir("workspace-old-");
    let (_d, recent_workspace) = mkdir("workspace-recent-");
    let (_e, recent_home) = mkdir("claude-recent-home-");
    let old_directory = claude.join("projects/-aaa-old");
    let old_transcript = old_directory.join("old.jsonl");
    let recent_directory = recent_home.join("projects/-zzz-recent");
    write_transcript(
        &old_transcript,
        &record(json!({"type": "user", "cwd": old_workspace, "sessionId": "old-session", "message": {"role": "user", "content": "Old work"}})),
        NOW_MS,
    );
    write_transcript(
        &recent_directory.join("recent.jsonl"),
        &record(json!({"type": "user", "cwd": recent_workspace, "sessionId": "recent-session", "message": {"role": "user", "content": "Recent work"}})),
        NOW_MS - 1000,
    );
    let simulated: Vec<String> = (0..5000).map(|i| format!("old-{i}.jsonl")).collect();
    let (od, ot) = (old_directory.clone(), old_transcript.clone());
    let resolve = Arc::new(move |path: &Path| -> PathBuf {
        if path.parent() == Some(od.as_path()) && path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("old-")) {
            ot.clone()
        } else {
            path.to_path_buf()
        }
    });
    let mut input = Input::new(&claude, &codex);
    input.provider_instances = json!({"claude-work": {"driver": "claudeAgent", "config": {"homePath": recent_home}}});
    let od = old_directory.clone();
    input.fs.dir = Some(Arc::new(move |directory| (directory == od).then(|| Ok(simulated.clone()))));
    let r = resolve.clone();
    input.fs.stat = Some(Arc::new(move |path| Some(RealFileSystem.stat(&r(path)))));
    let r = resolve.clone();
    input.fs.open = Some(Arc::new(move |path| Some(RealFileSystem.open(&r(path)))));
    let harness = harness(input).await;
    let scan = harness.scanner.scan().await.unwrap();
    assert_eq!(scan.truncated, Some(true));
    let threads: Vec<RecentThread> = harness.scanner.recent_threads(&s(&recent_workspace), Vec::new()).await.unwrap().collect().await;
    assert_eq!(
        importable(&threads).iter().map(|t| t.provider_session_id.as_str()).collect::<Vec<_>>(),
        ["recent-session"]
    );
}
