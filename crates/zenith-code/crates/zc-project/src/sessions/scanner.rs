//! `project/AgentSessionScanner.ts`: the projects a user already works on, discovered from the
//! transcripts Claude Code (`<home>/projects/*/*.jsonl`) and Codex
//! (`<home>/sessions/YYYY/MM/DD/rollout-*.jsonl`) keep on disk, and the recent sessions of one
//! project for import.
//!
//! Read-only and best effort: an unreadable home, a malformed transcript or a directory that
//! has since been deleted is skipped. Every source has budgets (discovery operations,
//! metadata bytes/reads/records) so a huge home cannot turn onboarding into a full disk scan;
//! hitting one sets `truncated`. zenith's own agent sandboxes (the worktrees directory, any
//! `.t3/worktrees`, the server's base directory), the home and temp roots, Downloads and
//! Codex's per-conversation scratch directories are never offered.
//!
//! The file system work runs on the blocking pool; directory listings are sorted like Node's
//! `readdir` (libuv sorts by bytes) and times are Node's (`Date` of the rounded `mtimeMs`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    AgentSessionImportSource, AgentSessionProjectCandidate, AgentSessionProjectGit, AgentSessionScanError, AgentSessionScanErrorOperation,
    AgentSessionScanResult, AgentSessionSource, JsNumber, LitAgentSessionScanError, ProviderInstanceId,
};
use zc_ports::ProjectionReads;

use super::fs::{FileStat, RealFileSystem, ScanFileSystem};
use super::json::TranscriptJsonReader;
use super::transcript::{
    decode_transcript_record, extract_decoded_cwd, parse_agent_session_records, select_transcript_path, should_retain_decoded_record, AgentSessionThread,
    TranscriptMetadata, TranscriptRecord, MAX_IMPORT_RECORDS,
};

/// Chunk size of full transcript reads.
const TRANSCRIPT_PREFIX_BYTES: usize = 32 * 1024;
/// Small reads keep long Codex instruction headers from wasting the metadata budget.
const METADATA_READ_BYTES: u64 = 8 * 1024;
/// A malformed transcript never turns discovery into a full file scan.
const MAX_TRANSCRIPT_SCAN_BYTES: u64 = 1024 * 1024;
/// Transcripts inspected per source (newest first, so the cap only drops stale sessions).
pub const MAX_TRANSCRIPTS_PER_SOURCE: usize = 5000;
/// Discovery file system operations per source (directory reads and stats).
pub const MAX_DISCOVERY_OPERATIONS_PER_SOURCE: usize = MAX_TRANSCRIPTS_PER_SOURCE * 4;
pub const MAX_METADATA_BYTES_PER_SOURCE: u64 = 64 * 1024 * 1024;
pub const MAX_METADATA_OPERATIONS_PER_SOURCE: usize = MAX_TRANSCRIPTS_PER_SOURCE * 4;
pub const MAX_METADATA_RECORDS_PER_SOURCE: usize = 100_000;
pub const MAX_METADATA_RECORDS_PER_TRANSCRIPT: usize = 1_000;
pub const RECENT_THREAD_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// Raw I/O cap per imported transcript (tool results can make one several GiB).
pub const MAX_IMPORTED_TRANSCRIPT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// Selected history kept in memory per transcript.
pub const MAX_IMPORT_HISTORY_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_IMPORT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_IMPORT_TRANSCRIPTS: usize = 100;

/// Where the server settings come from (`ServerSettingsService.getSettings`), encoded with
/// their record order.
#[async_trait]
pub trait SettingsSource: Send + Sync {
    async fn settings_value(&self) -> Result<Value, String>;
}

#[async_trait]
impl SettingsSource for zc_settings::ServerSettingsService {
    async fn settings_value(&self) -> Result<Value, String> {
        self.get_settings_value().await.map_err(|error| format!("{error:?}"))
    }
}

/// Fixed settings (tests).
pub struct StaticSettings(pub Value);

#[async_trait]
impl SettingsSource for StaticSettings {
    async fn settings_value(&self) -> Result<Value, String> {
        Ok(self.0.clone())
    }
}

/// What the scanner needs from the server's configuration and host.
#[derive(Clone)]
pub struct ScannerConfig {
    /// `ServerConfig.baseDir` (never offered as a project).
    pub base_dir: PathBuf,
    /// `ServerConfig.worktreesDir` (zenith's own agent sandboxes).
    pub worktrees_dir: PathBuf,
    pub home_dir: PathBuf,
    pub tmp_dir: PathBuf,
    /// `CLAUDE_CONFIG_DIR` / `CODEX_HOME` of the host environment.
    pub environment: HashMap<String, String>,
    /// Windows file systems fold case.
    pub fold_case: bool,
    /// `Date.now()`.
    pub now_millis: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The file system (tests observe it).
    pub fs: Arc<dyn ScanFileSystem>,
}

impl ScannerConfig {
    /// The host's home, temp directory and environment.
    pub fn new(base_dir: impl Into<PathBuf>, worktrees_dir: impl Into<PathBuf>) -> Self {
        let tmp = std::env::var("TMPDIR").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/tmp".into());
        let tmp = if tmp.len() > 1 { tmp.trim_end_matches('/').to_owned() } else { tmp };
        let environment = ["CLAUDE_CONFIG_DIR", "CODEX_HOME"]
            .into_iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.to_owned(), value)))
            .collect();
        Self {
            base_dir: base_dir.into(),
            worktrees_dir: worktrees_dir.into(),
            home_dir: zc_core::paths::home_dir(),
            tmp_dir: PathBuf::from(tmp),
            environment,
            fold_case: cfg!(windows),
            now_millis: Arc::new(zc_core::now_millis),
            fs: Arc::new(RealFileSystem),
        }
    }
}

/// `AgentSessionRecentThread`.
#[derive(Debug, Clone, PartialEq)]
pub enum RecentThread {
    Importable {
        thread: AgentSessionThread,
        source: AgentSessionImportSource,
    },
    AlreadyImported {
        source: AgentSessionImportSource,
    },
    Duplicate {
        source: AgentSessionImportSource,
    },
    Skipped,
}

#[derive(Debug, Clone, PartialEq)]
struct TranscriptCandidate {
    file_path: String,
    mtime_ms: i64,
    provider_instance_id: ProviderInstanceId,
    size: u64,
}

/// One directory's worth of evidence from one source.
#[derive(Debug, Clone, PartialEq)]
struct RawCandidate {
    cwd: String,
    source: AgentSessionSource,
    provider_instance_id: ProviderInstanceId,
    thread_count: usize,
    last_active_at_ms: Option<i64>,
    transcripts: Vec<(String, Option<i64>)>,
}

struct MetadataBudget {
    bytes_remaining: u64,
    operations_remaining: usize,
    records_remaining: usize,
    truncated: bool,
}

fn stat(fs: &dyn ScanFileSystem, path: &Path) -> Option<FileStat> {
    fs.stat(path).ok()
}

/// `readDirectory`; errors read as empty.
fn list_directory(fs: &dyn ScanFileSystem, directory: &Path) -> Vec<String> {
    fs.read_directory(directory).unwrap_or_default()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `path.resolve`.
fn resolve(path: &Path) -> PathBuf {
    zc_core::paths::resolve_path(path)
}

/// `expandHomePath` against the configured home.
fn expand_home(value: &str, home: &Path) -> PathBuf {
    zc_core::paths::expand_home_path_with(value, home)
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `normalizeProjectPathForComparison` (POSIX paths: trimmed, trailing separators dropped).
pub fn normalize_project_path_for_comparison(value: &str) -> String {
    let trimmed = js_trim(value);
    let is_drive = |text: &str| {
        let bytes = text.as_bytes();
        bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes.len() == 2 || matches!(bytes[2], b'/' | b'\\'))
    };
    let is_root = |text: &str| text == "/" || text == "\\" || (text.len() == 3 && is_drive(text));
    let normalized = if trimmed.is_empty() || is_root(trimmed) {
        trimmed.to_owned()
    } else {
        let stripped = if trimmed.starts_with('/') {
            trimmed.trim_end_matches('/')
        } else {
            trimmed.trim_end_matches(['/', '\\'])
        };
        if stripped.is_empty() {
            trimmed.to_owned()
        } else if stripped.len() == 2 && is_drive(stripped) {
            format!("{stripped}\\")
        } else {
            stripped.to_owned()
        }
    };
    if is_drive(&normalized) || normalized.starts_with("\\\\") {
        normalized.replace('/', "\\").to_lowercase()
    } else {
        normalized
    }
}

/// `normalizeForWorktreeMatch`.
fn normalize_for_match(value: &str, fold_case: bool) -> String {
    let normalized = format!("{}/", value.replace('\\', "/"));
    if fold_case {
        normalized.to_lowercase()
    } else {
        normalized
    }
}

/// `extractCwd`: the `cwd` of a session record (top level, or under Codex's `payload`).
fn extract_cwd(line: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(line).ok()?;
    let record = parsed.as_object()?;
    if let Some(Value::String(cwd)) = record.get("cwd") {
        if !js_trim(cwd).is_empty() {
            return Some(cwd.clone());
        }
    }
    if let Some(Value::String(cwd)) = record.get("payload").and_then(Value::as_object).and_then(|payload| payload.get("cwd")) {
        if !js_trim(cwd).is_empty() {
            return Some(cwd.clone());
        }
    }
    None
}

/// `transcriptIdentity`: `{filePath, size, mtimeMs, device, inode, birthtimeMs}`.
#[derive(Debug, Clone, PartialEq)]
struct TranscriptIdentity {
    file_path: String,
    size: i64,
    mtime_ms: Option<JsNumber>,
    device: JsNumber,
    inode: Option<JsNumber>,
    birthtime_ms: Option<JsNumber>,
}

fn transcript_identity(file_path: &str, stat: &FileStat) -> TranscriptIdentity {
    TranscriptIdentity {
        file_path: file_path.to_owned(),
        size: stat.size as i64,
        mtime_ms: stat.mtime_ms.map(JsNumber::from),
        device: JsNumber(stat.dev as f64),
        inode: Some(JsNumber(stat.ino as f64)),
        birthtime_ms: stat.birthtime_ms.map(JsNumber::from),
    }
}

fn same_identity(source: &AgentSessionImportSource, identity: &TranscriptIdentity) -> bool {
    source.file_path == identity.file_path
        && source.size == identity.size
        && source.mtime_ms == identity.mtime_ms
        && source.device == identity.device
        && source.inode == identity.inode
        && source.birthtime_ms == identity.birthtime_ms
}

fn scan_error(operation: AgentSessionScanErrorOperation, cause: impl std::fmt::Display) -> AgentSessionScanError {
    AgentSessionScanError {
        tag: LitAgentSessionScanError,
        operation,
        cause: json!({ "name": "Error", "message": cause.to_string() }),
    }
}

/// The host-side view the blocking work needs.
#[derive(Clone)]
struct Host {
    config: ScannerConfig,
    base_dir: String,
    worktrees_dir: String,
    excluded_roots: HashSet<String>,
    excluded_ancestors: Vec<String>,
}

impl Host {
    fn new(config: ScannerConfig) -> Self {
        let excluded_roots = [
            config.home_dir.clone(),
            config.tmp_dir.clone(),
            PathBuf::from("/tmp"),
            PathBuf::from("/private/tmp"),
        ]
        .iter()
        .map(|directory| normalize_project_path_for_comparison(&path_string(&resolve(directory))))
        .collect();
        // Codex makes one scratch directory per conversation under ~/Documents/Codex; nothing
        // unpacked into Downloads is a project either.
        let excluded_ancestors = vec![
            path_string(&config.home_dir.join("Downloads")),
            path_string(&config.home_dir.join("Documents").join("Codex")),
        ];
        Self {
            base_dir: path_string(&resolve(&config.base_dir)),
            worktrees_dir: path_string(&resolve(&config.worktrees_dir)),
            excluded_roots,
            excluded_ancestors,
            config,
        }
    }

    /// `isT3ManagedWorktree`.
    fn is_managed_worktree(&self, candidate: &str) -> bool {
        let fold = self.config.fold_case;
        let normalized = normalize_for_match(candidate, fold);
        normalized.starts_with(&normalize_for_match(&self.worktrees_dir, fold)) || normalized.contains("/.t3/worktrees/")
    }

    /// `isExcludedProjectPath`.
    fn is_excluded(&self, candidate: &str) -> bool {
        let fold = self.config.fold_case;
        let normalized = normalize_for_match(candidate, fold);
        self.excluded_roots.contains(&normalize_project_path_for_comparison(candidate))
            || self
                .excluded_ancestors
                .iter()
                .any(|ancestor| normalized.starts_with(&normalize_for_match(ancestor, fold)))
            || normalized.starts_with(&normalize_for_match(&self.base_dir, fold))
            || self.is_managed_worktree(candidate)
    }

    /// `directoryIdentity`: `inode:<dev>:<ino>`, else `path:<realpath>`.
    fn directory_identity(&self, target: &Path, known: Option<&FileStat>) -> String {
        let fs = self.config.fs.as_ref();
        let resolved = resolve(target);
        let stat = known.copied().or_else(|| stat(fs, &resolved));
        if let Some(stat) = stat.filter(|stat| stat.ino > 0 && stat.ino < (1u64 << 53)) {
            return format!("inode:{}:{}", stat.dev, stat.ino);
        }
        let real = fs.real_path(&resolved).unwrap_or(resolved);
        format!("path:{}", normalize_project_path_for_comparison(&path_string(&real)))
    }

    /// `readGitIdentity`.
    fn read_git_identity(&self, directory: &Path) -> GitIdentity {
        let fs = self.config.fs.as_ref();
        let git_path = directory.join(".git");
        let Some(git_stat) = stat(fs, &git_path) else {
            return GitIdentity::NotGit;
        };
        let mut git_dir = git_path.clone();
        if !git_stat.is_dir {
            let pointer = fs
                .read_file(&git_path)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            static GITDIR: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let pattern = GITDIR.get_or_init(|| regex::Regex::new(r"(?m)^gitdir:\s*(.+)$").expect("valid regex"));
            let Some(target) = pattern
                .captures(&pointer)
                .and_then(|c| c.get(1))
                .map(|m| js_trim(m.as_str()).to_owned())
                .filter(|t| !t.is_empty())
            else {
                return GitIdentity::NotGit;
            };
            git_dir = resolve(&directory.join(target));
            static WORKTREE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let worktree = WORKTREE.get_or_init(|| regex::Regex::new(r"[\\/]worktrees[\\/][^\\/]+[\\/]?$").expect("valid regex"));
            if worktree.is_match(&path_string(&git_dir)) {
                return GitIdentity::Worktree;
            }
        }
        let config = fs
            .read_file(&git_dir.join("config"))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let origin = parse_origin_url_from_git_config(&config);
        GitIdentity::Repository(AgentSessionProjectGit {
            remote_key: origin.as_deref().map(zc_vcs::shared_git::normalize_git_remote_url),
            repository: parse_github_repository_name_with_owner_from_remote_url(origin.as_deref()),
        })
    }
}

enum GitIdentity {
    Repository(AgentSessionProjectGit),
    Worktree,
    NotGit,
}

/// `parseGitConfigValue`.
fn parse_git_config_value(raw: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
                continue;
            }
            out.push(c);
            continue;
        }
        if c == '"' {
            quoted = !quoted;
            continue;
        }
        if !quoted && (c == '#' || c == ';') {
            break;
        }
        out.push(c);
    }
    js_trim(&out).to_owned()
}

/// `parseOriginUrlFromGitConfig`: `remote.origin.url`, else the first remote's url.
pub fn parse_origin_url_from_git_config(config_text: &str) -> Option<String> {
    static CONTINUATION: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static HEADER: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static URL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let continuation = CONTINUATION.get_or_init(|| regex::Regex::new(r"\\\r?\n[ \t]*").expect("valid regex"));
    let header = HEADER.get_or_init(|| regex::Regex::new(r#"(?i)^\[\s*remote(?:\s+"([^"]+)"|\.([^\]\s]+))\s*\](?:\s*[#;].*)?$"#).expect("valid regex"));
    let url_line = URL.get_or_init(|| regex::Regex::new(r"(?i)^url\s*=\s*(.*)$").expect("valid regex"));
    let joined = continuation.replace_all(config_text, "");
    let mut section: Option<String> = None;
    let mut origin: Option<String> = None;
    let mut first: Option<String> = None;
    for raw in joined.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let line = js_trim(raw);
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(captures) = header.captures(line) {
            section = captures
                .get(1)
                .map(|m| m.as_str().to_owned())
                .or_else(|| captures.get(2).map(|m| m.as_str().to_lowercase()));
            continue;
        }
        if line.starts_with('[') {
            section = None;
            continue;
        }
        let Some(section) = section.as_deref() else {
            continue;
        };
        let Some(captures) = url_line.captures(line) else {
            continue;
        };
        let url = parse_git_config_value(captures.get(1).map_or("", |m| m.as_str()));
        if url.is_empty() {
            continue;
        }
        if section == "origin" {
            origin.get_or_insert(url);
        } else {
            first.get_or_insert(url);
        }
    }
    origin.or(first)
}

/// `parseGitHubRepositoryNameWithOwnerFromRemoteUrl`.
pub fn parse_github_repository_name_with_owner_from_remote_url(url: Option<&str>) -> Option<String> {
    let trimmed = js_trim(url.unwrap_or_default());
    if trimmed.is_empty() {
        return None;
    }
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)^(?:git@github\.com:|ssh://(?:git@)?github\.com/|https://github\.com/|git://github\.com/)([^/\s]+/[^/\s]+?)(?:\.git)?/?$")
            .expect("valid regex")
    });
    let name = pattern.captures(trimmed).and_then(|c| c.get(1)).map(|m| js_trim(m.as_str()).to_owned())?;
    (!name.is_empty()).then_some(name)
}

/// `readCwd`: bounded chunks until a complete record names its `cwd` or the budget ends.
fn read_cwd(fs: &dyn ScanFileSystem, transcript: &TranscriptCandidate, budget: &mut MetadataBudget) -> Option<String> {
    if transcript.size == 0 {
        return None;
    }
    if budget.bytes_remaining == 0 || budget.operations_remaining < 2 || budget.records_remaining == 0 {
        budget.truncated = true;
        return None;
    }
    budget.operations_remaining -= 1;
    let mut file = fs.open(Path::new(&transcript.file_path)).ok()?;
    let mut decoder = Utf8Stream::default();
    let mut remaining = String::new();
    let mut bytes_read: u64 = 0;
    let mut records_read = 0usize;
    let max_bytes = MAX_TRANSCRIPT_SCAN_BYTES.min(transcript.size);
    let reserve_record = |budget: &mut MetadataBudget, records_read: &mut usize| {
        if *records_read == MAX_METADATA_RECORDS_PER_TRANSCRIPT || budget.records_remaining == 0 {
            budget.truncated = true;
            return false;
        }
        *records_read += 1;
        budget.records_remaining -= 1;
        true
    };
    let read_last = |budget: &mut MetadataBudget, records_read: &mut usize, remaining: &str, decoder: &mut Utf8Stream| {
        let record = format!("{remaining}{}", decoder.flush());
        if record.is_empty() || !reserve_record(budget, records_read) {
            return None;
        }
        extract_cwd(js_trim(&record))
    };
    while bytes_read < max_bytes {
        if budget.bytes_remaining == 0 || budget.operations_remaining == 0 {
            budget.truncated = true;
            return None;
        }
        let read_size = METADATA_READ_BYTES.min(max_bytes - bytes_read).min(budget.bytes_remaining);
        budget.operations_remaining -= 1;
        budget.bytes_remaining -= read_size;
        let Some(chunk) = file.read_alloc(read_size as usize).ok()? else {
            return read_last(budget, &mut records_read, &remaining, &mut decoder);
        };
        bytes_read += chunk.len() as u64;
        remaining.push_str(&decoder.decode(&chunk));
        let mut lines: Vec<String> = remaining.split('\n').map(str::to_owned).collect();
        remaining = lines.pop().unwrap_or_default();
        for line in lines {
            if !reserve_record(budget, &mut records_read) {
                return None;
            }
            if let Some(cwd) = extract_cwd(js_trim(&line)) {
                return Some(cwd);
            }
        }
    }
    if bytes_read < transcript.size {
        budget.truncated = true;
        return None;
    }
    read_last(budget, &mut records_read, &remaining, &mut decoder)
}

/// `TextDecoder` with `{stream: true}`: invalid sequences become U+FFFD, an incomplete one at
/// the end waits for the next chunk.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let pending = std::mem::take(&mut self.pending);
        let mut out = String::new();
        let mut rest: &[u8] = &pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
                    match error.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            rest = &rest[valid + len..];
                        }
                        None => {
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    fn flush(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        String::from_utf8_lossy(&pending).into_owned()
    }
}

/// The records of a transcript read for import.
struct TranscriptSnapshot {
    records: Vec<TranscriptRecord>,
    record_count: usize,
}

/// `readTranscript`: project history fields while reading; the file identity is checked on
/// both sides of the read; a selected-history budget failure rejects the transcript.
fn read_transcript(
    fs: &dyn ScanFileSystem,
    file_path: &str,
    expected: &TranscriptIdentity,
    record_limit: usize,
    source: AgentSessionSource,
) -> Option<TranscriptSnapshot> {
    if expected.size as u64 > MAX_IMPORTED_TRANSCRIPT_BYTES {
        return None;
    }
    let result = (|| -> Result<Option<TranscriptSnapshot>, String> {
        let mut file = fs.open(Path::new(file_path)).map_err(|e| e.to_string())?;
        let opened = file.stat().map_err(|e| e.to_string())?;
        if transcript_identity(file_path, &opened) != *expected {
            return Ok(None);
        }
        let select: &dyn Fn(&[super::json::PathSegment]) -> bool = &select_transcript_path;
        let mut records = Vec::new();
        let mut history_bytes = 0usize;
        let mut record_count = 0usize;
        let mut bytes_read: u64 = 0;
        let size = expected.size as u64;
        let mut reader = Some(TranscriptJsonReader::new(MAX_IMPORT_HISTORY_BYTES, select));
        let mut record_started = false;
        // `finishRecord`: false once the record limit is exceeded.
        let finish_record = |reader: &mut Option<TranscriptJsonReader<'_>>,
                             records: &mut Vec<TranscriptRecord>,
                             history_bytes: &mut usize,
                             record_count: &mut usize|
         -> Result<bool, String> {
            *record_count += 1;
            if *record_count > record_limit {
                return Ok(false);
            }
            let current = reader.take().expect("a reader per record");
            let charged = current.charged();
            let value = current.finish().map_err(|e| e.to_string())?;
            if let Some(record) = value.as_ref().and_then(decode_transcript_record) {
                if should_retain_decoded_record(source, &record) {
                    records.push(record);
                    *history_bytes += charged;
                }
            }
            *reader = Some(TranscriptJsonReader::new(MAX_IMPORT_HISTORY_BYTES.saturating_sub(*history_bytes), select));
            Ok(true)
        };
        while bytes_read < size {
            let want = (TRANSCRIPT_PREFIX_BYTES as u64).min(size - bytes_read) as usize;
            let Some(chunk) = file.read_alloc(want).map_err(|e| e.to_string())? else {
                return Ok(None);
            };
            bytes_read += chunk.len() as u64;
            let chunk = &chunk[..];
            let mut start = 0;
            while start < chunk.len() {
                let newline = chunk[start..].iter().position(|b| *b == b'\n').map(|i| start + i);
                let end = newline.unwrap_or(chunk.len());
                record_started = true;
                reader.as_mut().expect("a reader").write(&chunk[start..end]).map_err(|e| e.to_string())?;
                let Some(newline) = newline else {
                    break;
                };
                if !finish_record(&mut reader, &mut records, &mut history_bytes, &mut record_count)? {
                    return Ok(None);
                }
                record_started = false;
                start = newline + 1;
            }
        }
        if record_started && !finish_record(&mut reader, &mut records, &mut history_bytes, &mut record_count)? {
            return Ok(None);
        }
        let closed = file.stat().map_err(|e| e.to_string())?;
        if transcript_identity(file_path, &closed) != *expected {
            return Ok(None);
        }
        Ok(Some(TranscriptSnapshot { records, record_count }))
    })();
    match result {
        Ok(snapshot) => snapshot,
        Err(cause) => {
            tracing::warn!(file_path, cause, "Could not read imported transcript");
            None
        }
    }
}

/// `selectMetadataTranscripts`: one file per account per round, newest first, up to the cap.
fn select_metadata_transcripts(transcripts: &[TranscriptCandidate]) -> Vec<TranscriptCandidate> {
    let mut groups: Vec<(ProviderInstanceId, std::collections::VecDeque<TranscriptCandidate>)> = Vec::new();
    for transcript in transcripts {
        match groups.iter_mut().find(|(id, _)| *id == transcript.provider_instance_id) {
            Some((_, queue)) => queue.push_back(transcript.clone()),
            None => groups.push((transcript.provider_instance_id.clone(), [transcript.clone()].into())),
        }
    }
    let mut selected = Vec::new();
    while !groups.is_empty() && selected.len() < MAX_TRANSCRIPTS_PER_SOURCE {
        let mut next_round = Vec::new();
        let mut full = false;
        for (id, mut queue) in groups {
            if full || selected.len() == MAX_TRANSCRIPTS_PER_SOURCE {
                full = true;
                continue;
            }
            let Some(next) = queue.pop_front() else {
                continue;
            };
            selected.push(next);
            next_round.push((id, queue));
        }
        groups = next_round;
    }
    selected
}

/// A home to scan, owned by a provider instance.
struct Home {
    path: PathBuf,
    provider_instance_id: ProviderInstanceId,
}

struct Discovered {
    transcripts: Vec<TranscriptCandidate>,
    truncated: bool,
}

struct OperationBudget<'a> {
    fs: &'a dyn ScanFileSystem,
    remaining: usize,
    truncated: bool,
}

impl OperationBudget<'_> {
    fn read_directory(&mut self, directory: &Path) -> Vec<String> {
        if self.remaining == 0 {
            self.truncated = true;
            return Vec::new();
        }
        self.remaining -= 1;
        list_directory(self.fs, directory)
    }
}

fn candidate_of(file_path: String, stat: &FileStat, provider_instance_id: &ProviderInstanceId) -> Option<TranscriptCandidate> {
    (stat.is_file && stat.mtime_ms.is_some()).then(|| TranscriptCandidate {
        file_path,
        mtime_ms: stat.mtime_ms.unwrap_or_default(),
        provider_instance_id: provider_instance_id.clone(),
        size: stat.size,
    })
}

/// `discoverClaudeTranscripts`.
fn discover_claude(fs: &dyn ScanFileSystem, home: &Home, operation_budget: usize) -> Discovered {
    let projects_dir = home.path.join("projects");
    let mut budget = OperationBudget {
        fs,
        remaining: operation_budget,
        truncated: false,
    };
    let mut transcripts = Vec::new();
    for project_directory in budget.read_directory(&projects_dir) {
        if budget.remaining == 0 {
            budget.truncated = true;
            break;
        }
        let directory = projects_dir.join(&project_directory);
        let files: Vec<PathBuf> = budget
            .read_directory(&directory)
            .into_iter()
            .filter(|entry| entry.ends_with(".jsonl"))
            .map(|entry| directory.join(entry))
            .collect();
        for file_path in files {
            if budget.remaining == 0 {
                budget.truncated = true;
                break;
            }
            budget.remaining -= 1;
            if let Some(candidate) = stat(fs, &file_path).and_then(|stat| candidate_of(path_string(&file_path), &stat, &home.provider_instance_id)) {
                transcripts.push(candidate);
            }
        }
    }
    Discovered {
        transcripts,
        truncated: budget.truncated,
    }
}

/// `discoverCodexTranscripts`: date directories newest first.
fn discover_codex(fs: &dyn ScanFileSystem, home: &Home, operation_budget: usize) -> Discovered {
    let sessions_dir = home.path.join("sessions");
    let mut budget = OperationBudget {
        fs,
        remaining: operation_budget,
        truncated: false,
    };
    let mut transcripts = Vec::new();
    let reversed = |mut names: Vec<String>| {
        names.sort();
        names.reverse();
        names
    };
    for year in reversed(budget.read_directory(&sessions_dir)) {
        if budget.remaining == 0 {
            budget.truncated = true;
            break;
        }
        for month in reversed(budget.read_directory(&sessions_dir.join(&year))) {
            if budget.remaining == 0 {
                budget.truncated = true;
                break;
            }
            for day in reversed(budget.read_directory(&sessions_dir.join(&year).join(&month))) {
                if budget.remaining == 0 {
                    budget.truncated = true;
                    break;
                }
                let directory = sessions_dir.join(&year).join(&month).join(&day);
                for entry in reversed(budget.read_directory(&directory)) {
                    if !entry.starts_with("rollout-") || !entry.ends_with(".jsonl") {
                        continue;
                    }
                    if budget.remaining == 0 {
                        budget.truncated = true;
                        break;
                    }
                    let file_path = directory.join(&entry);
                    budget.remaining -= 1;
                    if let Some(candidate) = stat(fs, &file_path).and_then(|stat| candidate_of(path_string(&file_path), &stat, &home.provider_instance_id)) {
                        transcripts.push(candidate);
                    }
                }
            }
        }
    }
    Discovered {
        transcripts,
        truncated: budget.truncated,
    }
}

/// `groupTranscriptsByCwd`.
fn group_by_cwd(fs: &dyn ScanFileSystem, source: AgentSessionSource, transcripts: &[TranscriptCandidate], budget: &mut MetadataBudget) -> Vec<RawCandidate> {
    let mut groups: Vec<(String, RawCandidate)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for transcript in transcripts {
        let Some(cwd) = read_cwd(fs, transcript, budget) else {
            continue;
        };
        let key = format!("{}\0{cwd}", transcript.provider_instance_id);
        match index.get(&key) {
            Some(position) => {
                let group = &mut groups[*position].1;
                group.last_active_at_ms = Some(group.last_active_at_ms.unwrap_or(i64::MIN).max(transcript.mtime_ms));
                group.thread_count += 1;
                group.transcripts.push((transcript.file_path.clone(), Some(transcript.mtime_ms)));
            }
            None => {
                index.insert(key.clone(), groups.len());
                groups.push((
                    key,
                    RawCandidate {
                        cwd,
                        source,
                        provider_instance_id: transcript.provider_instance_id.clone(),
                        thread_count: 1,
                        last_active_at_ms: Some(transcript.mtime_ms),
                        transcripts: vec![(transcript.file_path.clone(), Some(transcript.mtime_ms))],
                    },
                ));
            }
        }
    }
    groups.into_iter().map(|(_, candidate)| candidate).collect()
}

/// The instances of one source whose homes are scanned (`collectCandidates`), built-in first.
fn source_homes(host: &Host, settings: &Value, source: AgentSessionSource) -> Vec<Home> {
    let source_key = source.as_str();
    let instances_map = settings.get("providerInstances").and_then(Value::as_object);
    let mut instances: Vec<(String, Value)> = instances_map
        .map(|map| {
            map.iter()
                .filter(|(_, instance)| {
                    instance.get("driver").and_then(Value::as_str) == Some(source_key)
                        && zc_settings::settings::logic::resolve_provider_instance_enabled(instance)
                })
                .map(|(id, instance)| (id.clone(), instance.clone()))
                .collect()
        })
        .unwrap_or_default();
    if !instances_map.is_some_and(|map| map.contains_key(source_key)) {
        let legacy = json!({
            "driver": source_key,
            "config": settings.get("providers").and_then(|p| p.get(source_key)).cloned().unwrap_or(Value::Null),
        });
        if zc_settings::settings::logic::resolve_provider_instance_enabled(&legacy) {
            instances.push((source_key.to_owned(), legacy));
        }
    }
    // A shared home holds one copy of each session: the built-in instance owns it, then the
    // configured order.
    instances.sort_by_key(|(id, _)| if id == source_key { 0 } else { 1 });
    let home_variable = if source == AgentSessionSource::ClaudeAgent {
        "CLAUDE_CONFIG_DIR"
    } else {
        "CODEX_HOME"
    };
    let home_dir = &host.config.home_dir;
    let mut homes = Vec::new();
    let mut seen = HashSet::new();
    for (instance_id, instance) in instances {
        let from_instance = instance
            .get("environment")
            .and_then(Value::as_array)
            .and_then(|variables| {
                variables
                    .iter()
                    .rev()
                    .find(|variable| variable.get("name").and_then(Value::as_str) == Some(home_variable))
            })
            .map(|variable| variable.get("value").and_then(Value::as_str).map(str::to_owned));
        let environment_home: Option<String> = match from_instance {
            Some(Some(value)) => Some(value),
            _ => host.config.environment.get(home_variable).cloned(),
        };
        let config = match instance.get("config") {
            None | Some(Value::Null) => json!({}),
            Some(config) => config.clone(),
        };
        let home_path = if source == AgentSessionSource::ClaudeAgent {
            let Ok(settings) = serde_json::from_value::<zc_contracts::ClaudeSettings>(config) else {
                continue;
            };
            // The precedence the spawned CLI sees: the instance's homePath, then the
            // environment's CLAUDE_CONFIG_DIR, then ~/.claude.
            let configured = js_trim(&settings.home_path);
            let from_environment = environment_home.as_deref().map(js_trim).unwrap_or_default();
            if !configured.is_empty() {
                resolve(&expand_home(configured, home_dir))
            } else if !from_environment.is_empty() {
                resolve(&expand_home(from_environment, home_dir))
            } else {
                home_dir.join(".claude")
            }
        } else {
            let Ok(settings) = serde_json::from_value::<zc_contracts::CodexSettings>(config) else {
                continue;
            };
            let home_setting = js_trim(&settings.home_path).to_owned();
            let use_environment = home_setting.is_empty()
                && js_trim(&settings.shadow_home_path).is_empty()
                && environment_home.as_deref().is_some_and(|value| !js_trim(value).is_empty());
            let home_setting = if use_environment {
                environment_home.clone().unwrap_or_default()
            } else {
                home_setting
            };
            // `resolveCodexHomeLayout(...).sharedHomePath`.
            if js_trim(&home_setting).is_empty() {
                resolve(&home_dir.join(".codex"))
            } else {
                resolve(&expand_home(&home_setting, home_dir))
            }
        };
        let key = format!("{source_key}\0{}", host.directory_identity(&home_path, None));
        if !seen.insert(key) {
            continue;
        }
        homes.push(Home {
            path: home_path,
            provider_instance_id: ProviderInstanceId::new(instance_id),
        });
    }
    homes
}

/// `collectCandidates` (blocking).
fn collect_candidates(host: &Host, settings: &Value) -> (Vec<RawCandidate>, bool) {
    let mut raw = Vec::new();
    let mut truncated = false;
    for source in [AgentSessionSource::ClaudeAgent, AgentSessionSource::Codex] {
        let homes = source_homes(host, settings, source);
        let count = homes.len().max(1);
        let base = MAX_DISCOVERY_OPERATIONS_PER_SOURCE / count;
        let extra = MAX_DISCOVERY_OPERATIONS_PER_SOURCE % count;
        let mut candidates: Vec<TranscriptCandidate> = Vec::new();
        for (index, home) in homes.iter().enumerate() {
            let budget = base + usize::from(index < extra);
            if budget == 0 {
                truncated = true;
                continue;
            }
            let fs = host.config.fs.as_ref();
            let discovered = if source == AgentSessionSource::ClaudeAgent {
                discover_claude(fs, home, budget)
            } else {
                discover_codex(fs, home, budget)
            };
            truncated |= discovered.truncated;
            candidates.extend(discovered.transcripts);
        }
        candidates.sort_by(|left, right| {
            right
                .mtime_ms
                .cmp(&left.mtime_ms)
                .then_with(|| zc_db::collate::locale_compare(&left.file_path, &right.file_path))
        });
        if candidates.len() > MAX_TRANSCRIPTS_PER_SOURCE {
            truncated = true;
        }
        let selected = select_metadata_transcripts(&candidates);
        let mut budget = MetadataBudget {
            bytes_remaining: MAX_METADATA_BYTES_PER_SOURCE,
            operations_remaining: MAX_METADATA_OPERATIONS_PER_SOURCE,
            records_remaining: MAX_METADATA_RECORDS_PER_SOURCE,
            truncated: false,
        };
        raw.extend(group_by_cwd(host.config.fs.as_ref(), source, &selected, &mut budget));
        truncated |= budget.truncated;
    }
    (raw, truncated)
}

struct Merged {
    path: String,
    sources: Vec<AgentSessionSource>,
    thread_count: usize,
    last_active_at_ms: Option<i64>,
    git: Option<AgentSessionProjectGit>,
}

/// The directories of a scan, merged by file system identity (blocking).
fn merge_candidates(host: &Host, raw: &[RawCandidate]) -> Vec<(String, Merged)> {
    let mut merged: Vec<(String, Merged)> = Vec::new();
    let mut directory_keys: HashMap<String, String> = HashMap::new();
    let mut git_identities: HashMap<String, Option<AgentSessionProjectGit>> = HashMap::new();
    let home = &host.config.home_dir;
    for candidate in raw {
        let expanded = expand_home(js_trim(&candidate.cwd), home);
        if !expanded.is_absolute() {
            continue;
        }
        let resolved = path_string(&resolve(&expanded));
        if host.is_excluded(&resolved) {
            continue;
        }
        let key = match directory_keys.get(&resolved) {
            Some(key) => key.clone(),
            None => {
                let key = match stat(host.config.fs.as_ref(), Path::new(&resolved)).filter(|stat| stat.is_dir) {
                    // Directories that no longer exist cannot be imported.
                    None => String::new(),
                    Some(stats) => {
                        let real = host
                            .config
                            .fs
                            .real_path(Path::new(&resolved))
                            .map(|p| path_string(&p))
                            .unwrap_or_else(|_| resolved.clone());
                        // A symlink can point into the worktrees directory even when its own
                        // spelling does not.
                        if host.is_excluded(&real) {
                            String::new()
                        } else {
                            match host.read_git_identity(Path::new(&resolved)) {
                                GitIdentity::Worktree => String::new(),
                                identity => {
                                    let key = host.directory_identity(Path::new(&resolved), Some(&stats));
                                    git_identities.insert(
                                        key.clone(),
                                        match identity {
                                            GitIdentity::Repository(git) => Some(git),
                                            _ => None,
                                        },
                                    );
                                    key
                                }
                            }
                        }
                    }
                };
                directory_keys.insert(resolved.clone(), key.clone());
                key
            }
        };
        if key.is_empty() {
            continue;
        }
        match merged.iter_mut().find(|(existing, _)| *existing == key) {
            None => merged.push((
                key.clone(),
                Merged {
                    path: resolved,
                    sources: vec![candidate.source],
                    thread_count: candidate.thread_count,
                    last_active_at_ms: candidate.last_active_at_ms,
                    git: git_identities.get(&key).cloned().flatten(),
                },
            )),
            Some((_, existing)) => {
                if !existing.sources.contains(&candidate.source) {
                    existing.sources.push(candidate.source);
                }
                existing.thread_count += candidate.thread_count;
                existing.last_active_at_ms = match (existing.last_active_at_ms, candidate.last_active_at_ms) {
                    (Some(left), Some(right)) => Some(left.max(right)),
                    (left, right) => left.or(right),
                };
            }
        }
    }
    merged
}

/// `path.basename`.
fn basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or_default().to_owned()
}

struct Inner {
    host: Host,
    settings: Arc<dyn SettingsSource>,
    reads: Arc<dyn ProjectionReads>,
    cached: Mutex<Option<Arc<Vec<RawCandidate>>>>,
    /// Imports can arrive concurrently from several clients: one transcript holds its
    /// selected-history budget at a time.
    import_lock: Arc<tokio::sync::Mutex<()>>,
}

/// The `AgentSessionScanner` service. Cloning shares it (and its candidate cache).
#[derive(Clone)]
pub struct AgentSessionScanner {
    inner: Arc<Inner>,
}

impl AgentSessionScanner {
    pub fn new(config: ScannerConfig, settings: Arc<dyn SettingsSource>, reads: Arc<dyn ProjectionReads>) -> Self {
        Self {
            inner: Arc::new(Inner {
                host: Host::new(config),
                settings,
                reads,
                cached: Mutex::new(None),
                import_lock: Arc::new(tokio::sync::Mutex::new(())),
            }),
        }
    }

    async fn collect(&self) -> Result<(Arc<Vec<RawCandidate>>, bool), AgentSessionScanError> {
        let settings = self
            .inner
            .settings
            .settings_value()
            .await
            .map_err(|cause| scan_error(AgentSessionScanErrorOperation::ReadSettings, cause))?;
        let host = self.inner.host.clone();
        let (raw, truncated) = tokio::task::spawn_blocking(move || collect_candidates(&host, &settings))
            .await
            .map_err(|cause| scan_error(AgentSessionScanErrorOperation::ReadSettings, cause))?;
        Ok((Arc::new(raw), truncated))
    }

    /// `scan`: every directory the configured Claude and Codex homes ran a session in, newest
    /// first, flagged when already a project.
    pub async fn scan(&self) -> Result<AgentSessionScanResult, AgentSessionScanError> {
        let (raw, truncated) = self.collect().await?;
        *self.inner.cached.lock().unwrap_or_else(|p| p.into_inner()) = Some(raw.clone());
        let host = self.inner.host.clone();
        let merged = {
            let raw = raw.clone();
            let host = host.clone();
            tokio::task::spawn_blocking(move || merge_candidates(&host, &raw))
                .await
                .map_err(|cause| scan_error(AgentSessionScanErrorOperation::ReadProjects, cause))?
        };
        // Persisted roots resolve too: a project and a transcript can name different symlinks
        // to the same directory.
        let shell = self
            .inner
            .reads
            .get_shell_snapshot(false)
            .await
            .map_err(|cause| scan_error(AgentSessionScanErrorOperation::ReadProjects, format!("{cause:?}")))?;
        let projects: Vec<(String, String)> = shell
            .projects
            .iter()
            .map(|project| (project.id.to_string(), project.workspace_root.to_string()))
            .collect();
        let candidates = tokio::task::spawn_blocking(move || {
            let mut by_root: HashMap<String, (String, String)> = HashMap::new();
            for (id, workspace_root) in &projects {
                let root = resolve(&expand_home(workspace_root, &host.config.home_dir));
                by_root.insert(normalize_project_path_for_comparison(&path_string(&root)), (id.clone(), workspace_root.clone()));
                by_root.insert(host.directory_identity(&root, None), (id.clone(), workspace_root.clone()));
            }
            let mut candidates: Vec<AgentSessionProjectCandidate> = merged
                .into_iter()
                .map(|(key, entry)| {
                    let imported = by_root
                        .get(&normalize_project_path_for_comparison(&entry.path))
                        .or_else(|| by_root.get(&key))
                        .cloned();
                    let path = imported.as_ref().map_or_else(|| entry.path.clone(), |(_, root)| root.clone());
                    let title = Some(basename(&path)).filter(|t| !t.is_empty()).unwrap_or_else(|| path.clone());
                    AgentSessionProjectCandidate {
                        title,
                        project_id: imported.as_ref().map(|(id, _)| zc_contracts::ProjectId::new(id.clone())),
                        sources: entry.sources,
                        thread_count: entry.thread_count as i64,
                        last_active_at: entry.last_active_at_ms.and_then(zc_core::time::try_iso_from_millis),
                        already_imported: imported.is_some(),
                        git: Some(entry.git),
                        path,
                    }
                })
                .collect();
            // Newest first, undated last.
            candidates.sort_by(|left, right| match (&left.last_active_at, &right.last_active_at) {
                (l, r) if l == r => zc_db::collate::locale_compare(&left.path, &right.path),
                (None, _) => std::cmp::Ordering::Greater,
                (_, None) => std::cmp::Ordering::Less,
                (Some(l), Some(r)) => zc_db::collate::locale_compare(r, l),
            });
            candidates
        })
        .await
        .map_err(|cause| scan_error(AgentSessionScanErrorOperation::ReadProjects, cause))?;
        Ok(AgentSessionScanResult {
            candidates,
            scanned_at: zc_core::time::iso_from_millis((self.inner.host.config.now_millis)()),
            truncated: truncated.then_some(true),
        })
    }

    /// `recentThreads(workspaceRoot, completedSources)`: the sessions of the last 30 days that
    /// ran in this project, newest first, one outcome per transcript (lazily: a consumer that
    /// stops early reads nothing more).
    pub async fn recent_threads(
        &self,
        workspace_root: &str,
        completed_sources: Vec<AgentSessionImportSource>,
    ) -> Result<BoxStream<'static, RecentThread>, AgentSessionScanError> {
        let host = self.inner.host.clone();
        let home = host.config.home_dir.clone();
        let root = resolve(&expand_home(workspace_root, &home));
        let (excluded, root_identity) = {
            let host = host.clone();
            let root = root.clone();
            tokio::task::spawn_blocking(move || {
                let real = host.config.fs.real_path(&root).unwrap_or_else(|_| root.clone());
                let excluded = host.is_excluded(&path_string(&root)) || host.is_excluded(&path_string(&real));
                (excluded, host.directory_identity(&root, None))
            })
            .await
            .map_err(|cause| scan_error(AgentSessionScanErrorOperation::ReadProjects, cause))?
        };
        if excluded {
            return Ok(futures::stream::empty().boxed());
        }
        let now_ms = (host.config.now_millis)();
        let cutoff_ms = now_ms - RECENT_THREAD_WINDOW_MS;
        let cached = self.inner.cached.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let candidates = match cached {
            Some(candidates) => candidates,
            None => self.collect().await?.0,
        };
        *self.inner.cached.lock().unwrap_or_else(|p| p.into_inner()) = Some(candidates.clone());

        let eligible = {
            let host = host.clone();
            let root_identity = root_identity.clone();
            tokio::task::spawn_blocking(move || {
                let mut eligible: Vec<(RawCandidate, String, i64)> = Vec::new();
                for candidate in candidates.iter() {
                    let expanded = expand_home(js_trim(&candidate.cwd), &host.config.home_dir);
                    if !expanded.is_absolute() || host.directory_identity(&resolve(&expanded), None) != root_identity {
                        continue;
                    }
                    for (file_path, mtime) in &candidate.transcripts {
                        match mtime {
                            Some(mtime) if *mtime >= cutoff_ms && *mtime <= now_ms => eligible.push((candidate.clone(), file_path.clone(), *mtime)),
                            _ => {}
                        }
                    }
                }
                eligible.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| zc_db::collate::locale_compare(&left.1, &right.1)));
                eligible
            })
            .await
            .map_err(|cause| scan_error(AgentSessionScanErrorOperation::ReadProjects, cause))?
        };

        let mut completed_by_file: HashMap<String, Vec<AgentSessionImportSource>> = HashMap::new();
        for source in completed_sources {
            completed_by_file
                .entry(format!("{}\0{}", source.provider_instance_id, source.file_path))
                .or_default()
                .push(source);
        }
        let state = RecentState {
            host,
            root_identity,
            eligible: eligible.into_iter().collect(),
            completed_by_file: Arc::new(completed_by_file),
            imported_sessions: HashSet::new(),
            bytes_remaining: MAX_IMPORT_BYTES,
            transcripts_remaining: MAX_IMPORT_TRANSCRIPTS,
            records_remaining: MAX_IMPORT_RECORDS,
            lock: self.inner.import_lock.clone(),
        };
        Ok(futures::stream::unfold(state, |mut state| async move {
            loop {
                let (candidate, file_path, mtime_ms) = state.eligible.pop_front()?;
                let lock = state.lock.clone();
                let _guard = lock.lock().await;
                let (outcome, back) = tokio::task::spawn_blocking(move || {
                    let outcome = state.next_outcome(&candidate, &file_path, mtime_ms);
                    (outcome, state)
                })
                .await
                .ok()?;
                state = back;
                if let Some(outcome) = outcome {
                    return Some((outcome, state));
                }
            }
        })
        .boxed())
    }
}

struct RecentState {
    host: Host,
    root_identity: String,
    eligible: std::collections::VecDeque<(RawCandidate, String, i64)>,
    completed_by_file: Arc<HashMap<String, Vec<AgentSessionImportSource>>>,
    imported_sessions: HashSet<String>,
    bytes_remaining: u64,
    transcripts_remaining: usize,
    records_remaining: usize,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl RecentState {
    /// One transcript's outcome; `None` for a copy of a session already reported.
    fn next_outcome(&mut self, candidate: &RawCandidate, file_path: &str, mtime_ms: i64) -> Option<RecentThread> {
        let completed = self.completed_by_file.get(&format!("{}\0{file_path}", candidate.provider_instance_id)).cloned();
        if completed.is_none() && (self.transcripts_remaining == 0 || self.bytes_remaining == 0 || self.records_remaining == 0) {
            return Some(RecentThread::Skipped);
        }
        let fs = self.host.config.fs.clone();
        let Some(stats) = stat(fs.as_ref(), Path::new(file_path)).filter(|stat| stat.is_file) else {
            return Some(RecentThread::Skipped);
        };
        let identity = transcript_identity(file_path, &stats);
        if let Some(source) = completed.as_ref().and_then(|sources| {
            sources
                .iter()
                .find(|source| source.provider == candidate.source && same_identity(source, &identity))
        }) {
            let key = format!("{}\0{}", source.provider_instance_id, source.provider_session_id);
            if !self.imported_sessions.insert(key) {
                return None;
            }
            return Some(RecentThread::AlreadyImported { source: source.clone() });
        }
        let size = stats.size;
        if self.transcripts_remaining == 0 || self.records_remaining == 0 || size > MAX_IMPORTED_TRANSCRIPT_BYTES || size > self.bytes_remaining {
            return Some(RecentThread::Skipped);
        }
        // The whole file is reserved even if its read or parse fails.
        self.transcripts_remaining -= 1;
        self.bytes_remaining -= size;
        let Some(snapshot) = read_transcript(fs.as_ref(), file_path, &identity, self.records_remaining, candidate.source) else {
            return Some(RecentThread::Skipped);
        };
        self.records_remaining = self.records_remaining.saturating_sub(snapshot.record_count);
        // A stable replacement file can belong to another project than the cached candidate.
        let Some(snapshot_cwd) = snapshot.records.iter().find_map(extract_decoded_cwd) else {
            return Some(RecentThread::Skipped);
        };
        let expanded = expand_home(js_trim(&snapshot_cwd), &self.host.config.home_dir);
        if !expanded.is_absolute() || self.host.directory_identity(&resolve(&expanded), None) != self.root_identity {
            return Some(RecentThread::Skipped);
        }
        let fallback_session_id = {
            let name = basename(file_path);
            name.strip_suffix(".jsonl").map(str::to_owned).unwrap_or(name)
        };
        let Some(thread) = parse_agent_session_records(
            &TranscriptMetadata {
                source: candidate.source,
                provider_instance_id: candidate.provider_instance_id.clone(),
                fallback_session_id,
                last_active_at_ms: mtime_ms,
            },
            &snapshot.records,
        ) else {
            return Some(RecentThread::Skipped);
        };
        let source = AgentSessionImportSource {
            provider: thread.source,
            provider_instance_id: thread.provider_instance_id.clone(),
            provider_session_id: thread.provider_session_id.clone(),
            file_path: identity.file_path.clone(),
            size: identity.size,
            mtime_ms: identity.mtime_ms,
            device: identity.device,
            inode: identity.inode,
            birthtime_ms: identity.birthtime_ms,
        };
        let key = format!("{}\0{}", thread.provider_instance_id, thread.provider_session_id);
        if !self.imported_sessions.insert(key) {
            return Some(RecentThread::Duplicate { source });
        }
        Some(RecentThread::Importable { thread, source })
    }
}
