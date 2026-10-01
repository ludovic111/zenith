//! `GitVcsDriverCore.ts` / the `GitVcsDriver` service: the git driver the workflow, the status
//! readers, the review service and (later) GitManager use.
//!
//! Every git invocation (operation name, arguments, flags, environment, timeouts, output caps,
//! error details) is the TS one. Caches and their lifetimes:
//!
//! | Cache | Key | Lifetime |
//! |---|---|---|
//! | repository paths | normalized cwd | 10 min (1 s when not a repository), refresh coalesced 5 s |
//! | default branch, origin exists | git common dir | 5 min |
//! | background upstream fetch | (common dir, remote) | 15 s; failures back off 30 s → 15 min |
//! | ref snapshot | (common dir, epoch) | 2 min |
//! | ref snapshot refresh | (common dir, generation) | 5 s, failures 30 s |
//!
//! Every mutating method invalidates the ref snapshot and the status static caches of its cwd
//! when it ends, success or not (`withListRefsInvalidation`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;
use sha2::{Digest, Sha256};
use zc_core::defect::{js_length, Defect};
use zc_ports::contracts::WorktreeSubmodules;
use zc_ports::git::{CheckoutProgress, CreateWorktreeOptions, SubmodulesDisabledSource};

use crate::cache::{BoundedOrderMap, OutcomeCache};
use crate::collate::locale_compare;
use crate::contracts::*;
use crate::errors::{platform_error_defect, GitCommandError};
use crate::git_exec::{
    env, EnvOverlay, ExecuteGitInput, ExecuteGitProgress, ExecuteGitResult, GitExecutor, GitTimeout, HookFinished, LineCallback, OUTPUT_TRUNCATED_MARKER,
};
use crate::parse::*;
use crate::project_file::{parse_t3_project_file, resolve_worktree_submodules, SettingSource, T3_PROJECT_FILE_NAME};
use crate::remote_refs::{parse_remote_names, parse_remote_names_in_git_order, parse_remote_ref_with_remote_names, sort_longest_first};
use crate::shared_git::{dedupe_remote_branches_with_local_matches, normalize_git_remote_url};

const WORKTREE_ADD_TIMEOUT_MS: u64 = 300_000;
const WORKTREE_REMOVE_TIMEOUT_MS: u64 = 300_000;
const PREPARED_COMMIT_PATCH_MAX_OUTPUT_BYTES: usize = 49_000;
const RANGE_COMMIT_SUMMARY_MAX_OUTPUT_BYTES: usize = 19_000;
const RANGE_DIFF_SUMMARY_MAX_OUTPUT_BYTES: usize = 19_000;
const RANGE_DIFF_PATCH_MAX_OUTPUT_BYTES: usize = 59_000;
const REVIEW_DIFF_PATCH_MAX_OUTPUT_BYTES: usize = 120_000;
const REVIEW_METADATA_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Patches the clients render are parsed against git's default `a/` and `b/` prefixes.
pub const PATCH_RENDER_PREFIX_ARGS: [&str; 2] = ["--src-prefix=a/", "--dst-prefix=b/"];
const STATUS_UPSTREAM_REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const STATUS_UPSTREAM_REFRESH_TIMEOUT_MS: u64 = 5_000;
const STATUS_UPSTREAM_REFRESH_FAILURE_BASE_COOLDOWN: Duration = Duration::from_secs(30);
const STATUS_UPSTREAM_REFRESH_FAILURE_MAX_COOLDOWN: Duration = Duration::from_secs(15 * 60);
const STATUS_UPSTREAM_REFRESH_CACHE_CAPACITY: usize = 2_048;
const REPOSITORY_PATHS_CACHE_CAPACITY: usize = 2_048;
const REPOSITORY_PATHS_CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const REPOSITORY_PATHS_REFRESH_COALESCE_TTL: Duration = Duration::from_secs(5);
const NON_REPOSITORY_PATHS_CACHE_TTL: Duration = Duration::from_secs(1);
const LIST_REFS_SNAPSHOT_CACHE_CAPACITY: usize = 64;
const LIST_REFS_SNAPSHOT_CACHE_TTL: Duration = Duration::from_secs(2 * 60);
const LIST_REFS_REFRESH_COALESCE_TTL: Duration = Duration::from_secs(5);
const LIST_REFS_REFRESH_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
const STATUS_DEFAULT_BRANCH_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const STATUS_ORIGIN_EXISTS_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const DEFAULT_BASE_BRANCH_CANDIDATES: [&str; 2] = ["main", "master"];

/// `STATUS_UPSTREAM_REFRESH_ENV`: background and explicit fetches never prompt.
pub fn non_interactive_env() -> EnvOverlay {
    env([
        ("GCM_INTERACTIVE", "never"),
        ("GIT_ASKPASS", ""),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("SSH_ASKPASS", ""),
        ("SSH_ASKPASS_REQUIRE", "never"),
    ])
}

/// `statusUpstreamRefreshFailureCooldown`: 30 s doubling per consecutive failure, at most 15 min.
pub fn status_upstream_refresh_failure_cooldown(consecutive_failures: u32) -> Duration {
    let exponent = consecutive_failures.saturating_sub(1).min(20);
    let cooldown = STATUS_UPSTREAM_REFRESH_FAILURE_BASE_COOLDOWN.saturating_mul(1 << exponent);
    cooldown.min(STATUS_UPSTREAM_REFRESH_FAILURE_MAX_COOLDOWN)
}

// ---------------------------------------------------------------------------------------------
// Driver-level types (`GitVcsDriver.ts`)
// ---------------------------------------------------------------------------------------------

/// `GitStatusDetails`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusDetails {
    pub is_repo: bool,
    pub has_origin_remote: bool,
    pub is_default_branch: bool,
    pub branch: Option<String>,
    pub upstream_ref: Option<String>,
    pub has_working_tree_changes: bool,
    pub working_tree: WorkingTree,
    pub has_upstream: bool,
    pub ahead_count: u64,
    pub behind_count: u64,
    pub ahead_of_default_count: u64,
}

impl GitStatusDetails {
    /// `NON_REPOSITORY_STATUS_DETAILS`.
    pub fn non_repository() -> Self {
        Self {
            is_repo: false,
            has_origin_remote: false,
            is_default_branch: false,
            branch: None,
            upstream_ref: None,
            has_working_tree_changes: false,
            working_tree: WorkingTree::default(),
            has_upstream: false,
            ahead_count: 0,
            behind_count: 0,
            ahead_of_default_count: 0,
        }
    }
}

/// `GitRemoteStatusDetails`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRemoteStatusDetails {
    pub is_repo: bool,
    pub default_branch: Option<String>,
    pub is_default_branch: bool,
    pub branch: Option<String>,
    pub upstream_ref: Option<String>,
    pub has_upstream: bool,
    pub ahead_count: u64,
    pub behind_count: u64,
    pub ahead_of_default_count: u64,
}

impl GitRemoteStatusDetails {
    /// `NON_REPOSITORY_REMOTE_STATUS_DETAILS`.
    pub fn non_repository() -> Self {
        Self {
            is_repo: false,
            default_branch: None,
            is_default_branch: false,
            branch: None,
            upstream_ref: None,
            has_upstream: false,
            ahead_count: 0,
            behind_count: 0,
            ahead_of_default_count: 0,
        }
    }
}

/// `GitPreparedCommitContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPreparedCommitContext {
    pub staged_summary: String,
    pub staged_patch: String,
}

/// `stream` of `GitCommitProgress.onOutputLine`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// `GitCommitProgress`.
#[derive(Clone, Default)]
#[allow(clippy::type_complexity)]
pub struct GitCommitProgress {
    pub on_output_line: Option<Arc<dyn Fn(OutputStream, &str) + Send + Sync>>,
    pub on_hook_started: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    pub on_hook_finished: Option<Arc<dyn Fn(HookFinished) + Send + Sync>>,
}

/// `GitCommitOptions`.
#[derive(Clone, Default)]
pub struct GitCommitOptions {
    pub timeout_ms: Option<u64>,
    pub progress: Option<GitCommitProgress>,
}

/// `status` of `GitPushResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitPushStatus {
    Pushed,
    SkippedUpToDate,
}

/// `GitPushResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitPushResult {
    pub status: GitPushStatus,
    pub branch: String,
    pub upstream_branch: Option<String>,
    pub set_upstream: Option<bool>,
}

/// `GitRangeContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRangeContext {
    pub commit_summary: String,
    pub diff_summary: String,
    pub diff_patch: String,
}

/// `GitRefreshCheckedOutBranchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRefreshCheckedOutBranchResult {
    pub head_commit: String,
    pub moved: bool,
    pub on_target: bool,
}

/// `GitResolveRemoteTrackingCommitResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTrackingCommit {
    pub commit_sha: String,
    pub remote_ref_name: String,
}

/// What `resolveRepositoryPaths` learns about a cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRepositoryPaths {
    pub git_common_dir: String,
    pub worktree_root: Option<String>,
    pub current_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitRefsSnapshot {
    local_branches: Vec<VcsRef>,
    remote_branches: Vec<VcsRef>,
    has_primary_remote: bool,
}

/// `ExecuteGitOptions` of the core.
#[derive(Clone, Default)]
struct GitOpts {
    stdin: Option<String>,
    timeout: GitTimeout,
    allow_non_zero_exit: bool,
    fallback_error_detail: Option<String>,
    env: Option<EnvOverlay>,
    max_output_bytes: Option<usize>,
    append_truncation_marker: bool,
    progress: Option<ExecuteGitProgress>,
}

impl GitOpts {
    fn allow_non_zero() -> Self {
        Self {
            allow_non_zero_exit: true,
            ..Self::default()
        }
    }

    fn timeout_ms(mut self, ms: u64) -> Self {
        self.timeout = GitTimeout::Millis(ms);
        self
    }

    fn fallback(mut self, detail: &str) -> Self {
        self.fallback_error_detail = Some(detail.to_owned());
        self
    }

    fn with_env(mut self, overlay: EnvOverlay) -> Self {
        self.env = Some(overlay);
        self
    }

    fn max_bytes(mut self, max: usize) -> Self {
        self.max_output_bytes = Some(max);
        self
    }

    fn truncate(mut self) -> Self {
        self.append_truncation_marker = true;
        self
    }
}

fn s(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `path.normalize(path.resolve(value))`.
pub fn normalize_cwd_key(cwd: &str) -> String {
    path_string(&zc_core::paths::normalize_lexically(&zc_core::paths::resolve_path(Path::new(cwd))))
}

/// `path.resolve(base, value)` (normalized).
fn resolve_from(base: &str, value: &str) -> String {
    path_string(&crate::git_exec::resolve_against(base, value))
}

fn basename(path: &str) -> String {
    Path::new(path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The directory git commands of a common dir run from (`fetchCwd`).
fn fetch_cwd_for(git_common_dir: &str) -> String {
    if basename(git_common_dir) == ".git" {
        Path::new(git_common_dir).parent().map(path_string).unwrap_or_else(|| git_common_dir.to_owned())
    } else {
        git_common_dir.to_owned()
    }
}

async fn path_exists(path: impl AsRef<Path>) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

async fn real_path_or(path: &str) -> String {
    tokio::fs::canonicalize(path).await.map(|p| path_string(&p)).unwrap_or_else(|_| path.to_owned())
}

/// `path.relative(root, candidate)` stays inside `root`.
pub(crate) fn is_path_within_root(root: &Path, candidate: &Path) -> bool {
    let root = zc_core::paths::normalize_lexically(root);
    let candidate = zc_core::paths::normalize_lexically(candidate);
    candidate == root || candidate.starts_with(&root)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Seconds since the epoch of a file's mtime, with Node's millisecond truncation.
fn mtime_millis(meta: &std::fs::Metadata) -> Option<i64> {
    let modified = meta.modified().ok()?;
    let since = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(since.as_millis() as i64)
}

/// `fs.utimes(path, seconds, seconds)`.
fn set_file_times(path: &Path, seconds: i64) -> std::io::Result<()> {
    let time = std::time::UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64);
    let file = std::fs::OpenOptions::new().write(true).open(path)?;
    file.set_times(std::fs::FileTimes::new().set_accessed(time).set_modified(time))
}

/// A temporary file removed on drop (`makeTempFileScoped`).
struct TempFile(PathBuf);

impl TempFile {
    fn create(prefix: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("{prefix}{}", uuid::Uuid::new_v4().simple()));
        std::fs::File::create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------------------------

type PathsCache = OutcomeCache<String, Option<GitRepositoryPaths>, GitCommandError>;
type SnapshotCache = OutcomeCache<(String, u64), Arc<GitRefsSnapshot>, GitCommandError>;

#[derive(Default)]
struct ListRefsCounters {
    epoch_by_common_dir: Option<BoundedOrderMap<u64>>,
    epoch_sequence: u64,
    generation_by_common_dir: Option<BoundedOrderMap<u64>>,
    generation_sequence: u64,
}

impl ListRefsCounters {
    fn epochs(&mut self) -> &mut BoundedOrderMap<u64> {
        self.epoch_by_common_dir
            .get_or_insert_with(|| BoundedOrderMap::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY))
    }

    fn generations(&mut self) -> &mut BoundedOrderMap<u64> {
        self.generation_by_common_dir
            .get_or_insert_with(|| BoundedOrderMap::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY))
    }

    fn bump_epoch(&mut self, dir: &str) -> u64 {
        self.epoch_sequence += 1;
        let next = self.epoch_sequence;
        self.epochs().set(dir, next);
        next
    }

    fn current_generation(&mut self, dir: &str) -> u64 {
        match self.generations().get(dir) {
            Some(current) => {
                self.generations().set(dir, current);
                current
            }
            None => {
                self.generation_sequence += 1;
                let next = self.generation_sequence;
                self.generations().set(dir, next);
                next
            }
        }
    }

    fn bump_generation(&mut self, dir: &str) -> u64 {
        self.generation_sequence += 1;
        let next = self.generation_sequence;
        self.generations().set(dir, next);
        next
    }
}

struct Inner {
    exec: GitExecutor,
    worktrees_dir: PathBuf,
    repository_paths: PathsCache,
    repository_paths_refresh: PathsCache,
    default_branch: OutcomeCache<String, Option<String>, GitCommandError>,
    origin_exists: OutcomeCache<String, bool, GitCommandError>,
    remote_refresh_failures: Arc<Mutex<BoundedOrderMap<u32>>>,
    remote_refresh: OutcomeCache<(String, String), bool, GitCommandError>,
    list_refs_counters: Mutex<ListRefsCounters>,
    list_refs_snapshots: SnapshotCache,
    list_refs_refresh: SnapshotCache,
}

/// The git driver (`GitVcsDriver` service). Cheap to clone; clones share caches.
#[derive(Clone)]
pub struct GitVcsDriver {
    inner: Arc<Inner>,
}

fn failure_key(git_common_dir: &str, remote_name: &str) -> String {
    format!("{git_common_dir}\0{remote_name}")
}

impl GitVcsDriver {
    /// `makeGitVcsDriverCore` with `ServerConfig.worktreesDir`.
    pub fn new(worktrees_dir: impl Into<PathBuf>) -> Self {
        Self::with_executor(worktrees_dir, GitExecutor::new())
    }

    pub fn with_executor(worktrees_dir: impl Into<PathBuf>, exec: GitExecutor) -> Self {
        let failures: Arc<Mutex<BoundedOrderMap<u32>>> = Arc::new(Mutex::new(BoundedOrderMap::new(STATUS_UPSTREAM_REFRESH_CACHE_CAPACITY)));
        let ttl_failures = failures.clone();
        Self {
            inner: Arc::new(Inner {
                exec,
                worktrees_dir: worktrees_dir.into(),
                repository_paths: OutcomeCache::new(REPOSITORY_PATHS_CACHE_CAPACITY, |r, _| match r {
                    Ok(Some(_)) => REPOSITORY_PATHS_CACHE_TTL,
                    Ok(None) => NON_REPOSITORY_PATHS_CACHE_TTL,
                    Err(_) => Duration::ZERO,
                }),
                repository_paths_refresh: OutcomeCache::new(REPOSITORY_PATHS_CACHE_CAPACITY, |r, _| match r {
                    Ok(Some(_)) => REPOSITORY_PATHS_REFRESH_COALESCE_TTL,
                    Ok(None) => NON_REPOSITORY_PATHS_CACHE_TTL,
                    Err(_) => Duration::ZERO,
                }),
                default_branch: OutcomeCache::new(2_048, |r, _| if r.is_ok() { STATUS_DEFAULT_BRANCH_CACHE_TTL } else { Duration::ZERO }),
                origin_exists: OutcomeCache::new(2_048, |r, _| if r.is_ok() { STATUS_ORIGIN_EXISTS_CACHE_TTL } else { Duration::ZERO }),
                remote_refresh_failures: failures,
                remote_refresh: OutcomeCache::new(STATUS_UPSTREAM_REFRESH_CACHE_CAPACITY, move |r, (dir, remote): &(String, String)| {
                    if r.is_ok() {
                        STATUS_UPSTREAM_REFRESH_INTERVAL
                    } else {
                        let count = ttl_failures
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .get(&failure_key(dir, remote))
                            .unwrap_or(1);
                        status_upstream_refresh_failure_cooldown(count)
                    }
                }),
                list_refs_counters: Mutex::new(ListRefsCounters::default()),
                list_refs_snapshots: OutcomeCache::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY, |r, _| {
                    if r.is_ok() {
                        LIST_REFS_SNAPSHOT_CACHE_TTL
                    } else {
                        Duration::ZERO
                    }
                }),
                list_refs_refresh: OutcomeCache::new(LIST_REFS_SNAPSHOT_CACHE_CAPACITY, |r, _| {
                    if r.is_ok() {
                        LIST_REFS_REFRESH_COALESCE_TTL
                    } else {
                        LIST_REFS_REFRESH_FAILURE_COOLDOWN
                    }
                }),
            }),
        }
    }

    pub fn worktrees_dir(&self) -> &Path {
        &self.inner.worktrees_dir
    }

    // -----------------------------------------------------------------------------------------
    // Execution helpers
    // -----------------------------------------------------------------------------------------

    /// `execute`: the raw runner (no exit-code check unless `allow_non_zero_exit` is false).
    pub async fn execute(&self, input: ExecuteGitInput) -> Result<ExecuteGitResult, GitCommandError> {
        self.inner.exec.execute(input).await
    }

    /// `executeGit`: run with `allowNonZeroExit`, then fail non-zero exits with the fallback
    /// detail unless the options allow them.
    async fn git(&self, operation: &str, cwd: &str, args: Vec<String>, opts: GitOpts) -> Result<ExecuteGitResult, GitCommandError> {
        let argument_count = args.len();
        let result = self
            .execute(ExecuteGitInput {
                operation: operation.to_owned(),
                cwd: cwd.to_owned(),
                args,
                stdin: opts.stdin,
                env: opts.env,
                allow_non_zero_exit: true,
                timeout: opts.timeout,
                max_output_bytes: opts.max_output_bytes,
                append_truncation_marker: opts.append_truncation_marker,
                keep_line_callbacks_after_truncation: false,
                progress: opts.progress,
            })
            .await?;
        if opts.allow_non_zero_exit || result.exit_code == 0 {
            return Ok(result);
        }
        Err(GitCommandError {
            exit_code: Some(result.exit_code),
            stdout_length: Some(js_length(&result.stdout)),
            stderr_length: Some(js_length(&result.stderr)),
            ..GitCommandError::git(
                operation,
                cwd,
                argument_count,
                opts.fallback_error_detail.as_deref().unwrap_or("Git command exited with a non-zero status."),
            )
        })
    }

    /// `executeGitWithStableDiagnostics`: `LC_ALL=C` so stderr matching works in any locale.
    async fn git_c(&self, operation: &str, cwd: &str, args: Vec<String>, mut opts: GitOpts) -> Result<ExecuteGitResult, GitCommandError> {
        let mut overlay = opts.env.take().unwrap_or_default();
        overlay.insert("LC_ALL".into(), Some("C".into()));
        opts.env = Some(overlay);
        self.git(operation, cwd, args, opts).await
    }

    async fn run_git(&self, operation: &str, cwd: &str, args: Vec<String>, opts: GitOpts) -> Result<(), GitCommandError> {
        self.git(operation, cwd, args, opts).await.map(|_| ())
    }

    /// `runGitStdout`.
    async fn stdout(&self, operation: &str, cwd: &str, args: Vec<String>, allow_non_zero_exit: bool) -> Result<String, GitCommandError> {
        let opts = GitOpts {
            allow_non_zero_exit,
            ..GitOpts::default()
        };
        Ok(self.git(operation, cwd, args, opts).await?.stdout)
    }

    /// `runGitStdoutWithOptions`: appends the truncation marker to truncated output.
    async fn stdout_with(&self, operation: &str, cwd: &str, args: Vec<String>, opts: GitOpts) -> Result<String, GitCommandError> {
        let result = self.git(operation, cwd, args, opts).await?;
        Ok(if result.stdout_truncated {
            format!("{}{OUTPUT_TRUNCATED_MARKER}", result.stdout)
        } else {
            result.stdout
        })
    }

    async fn branch_exists(&self, cwd: &str, ref_name: &str) -> Result<bool, GitCommandError> {
        let result = self
            .git(
                "GitVcsDriver.branchExists",
                cwd,
                vec!["show-ref".into(), "--verify".into(), "--quiet".into(), format!("refs/heads/{ref_name}")],
                GitOpts::allow_non_zero().timeout_ms(5_000),
            )
            .await?;
        Ok(result.exit_code == 0)
    }

    async fn resolve_available_branch_name(&self, cwd: &str, desired: &str) -> Result<String, GitCommandError> {
        if !self.branch_exists(cwd, desired).await? {
            return Ok(desired.to_owned());
        }
        for suffix in 1..=100 {
            let candidate = format!("{desired}-{suffix}");
            if !self.branch_exists(cwd, &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(GitCommandError::git(
            "GitVcsDriver.renameBranch",
            cwd,
            4,
            format!("Could not find an available branch name for '{desired}'."),
        ))
    }

    /// `resolveCurrentUpstream`.
    pub async fn resolve_current_upstream(&self, cwd: &str) -> Result<Option<UpstreamRef>, GitCommandError> {
        let upstream_ref = self
            .stdout(
                "GitVcsDriver.resolveCurrentUpstream",
                cwd,
                s(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]),
                true,
            )
            .await?
            .trim()
            .to_owned();
        if upstream_ref.is_empty() || upstream_ref == "@{upstream}" {
            return Ok(None);
        }
        let remote_names = self
            .stdout("GitVcsDriver.listRemoteNames", cwd, s(&["remote"]), false)
            .await
            .map(|stdout| parse_remote_names(&stdout))
            .unwrap_or_default();
        Ok(parse_upstream_ref_with_remote_names(&upstream_ref, &remote_names).or_else(|| parse_upstream_ref_by_first_separator(&upstream_ref)))
    }

    async fn fetch_remote_for_status(&self, git_common_dir: &str, remote_name: &str) -> Result<(), GitCommandError> {
        // `--no-auto-gc` keeps a background poll from starting `git gc --auto`, which can leave
        // a full-size tmp pack behind on every failed attempt.
        self.run_git(
            "GitVcsDriver.fetchRemoteForStatus",
            &fetch_cwd_for(git_common_dir),
            vec![
                "--git-dir".into(),
                git_common_dir.into(),
                "fetch".into(),
                "--quiet".into(),
                "--no-tags".into(),
                "--no-auto-gc".into(),
                remote_name.into(),
            ],
            GitOpts::default()
                .with_env(non_interactive_env())
                .fallback("Background Git fetch exited with a non-zero status.")
                .timeout_ms(STATUS_UPSTREAM_REFRESH_TIMEOUT_MS),
        )
        .await
    }

    // -----------------------------------------------------------------------------------------
    // Repository paths and static status caches
    // -----------------------------------------------------------------------------------------

    async fn resolve_repository_paths_uncached(&self, cwd: &str) -> Result<Option<GitRepositoryPaths>, GitCommandError> {
        let operation = "GitVcsDriver.resolveRepositoryPaths.commonDir";
        let common = self
            .git_c(
                operation,
                cwd,
                s(&["rev-parse", "--git-common-dir"]),
                GitOpts::allow_non_zero().timeout_ms(5_000),
            )
            .await?;
        if common.exit_code != 0 {
            if is_non_repository_git_stderr(common.stderr.trim()) {
                return Ok(None);
            }
            return Err(GitCommandError {
                exit_code: Some(common.exit_code),
                stdout_length: Some(js_length(&common.stdout)),
                stderr_length: Some(js_length(&common.stderr)),
                ..GitCommandError::git(operation, cwd, 2, "Failed to resolve the Git common directory.")
            });
        }
        let resolved = resolve_from(cwd, common.stdout.trim());
        let git_common_dir = real_path_or(&resolved).await;
        let (root, branch) = tokio::try_join!(
            self.git(
                "GitVcsDriver.resolveRepositoryPaths.worktreeRoot",
                cwd,
                s(&["rev-parse", "--show-toplevel"]),
                GitOpts::allow_non_zero().timeout_ms(5_000),
            ),
            self.git(
                "GitVcsDriver.resolveRepositoryPaths.currentBranch",
                cwd,
                s(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
                GitOpts::allow_non_zero().timeout_ms(5_000),
            )
        )?;
        let root_output = root.stdout.trim();
        let worktree_root = (root.exit_code == 0 && !root_output.is_empty()).then(|| resolve_from(cwd, root_output));
        let branch_output = branch.stdout.trim();
        let current_branch = (branch.exit_code == 0 && !branch_output.is_empty()).then(|| branch_output.to_owned());
        Ok(Some(GitRepositoryPaths {
            git_common_dir,
            worktree_root,
            current_branch,
        }))
    }

    async fn repository_paths_cached(&self, key: String) -> Result<Option<GitRepositoryPaths>, GitCommandError> {
        let this = self.clone();
        let cwd = key.clone();
        self.inner
            .repository_paths
            .get(key, move || async move { this.resolve_repository_paths_uncached(&cwd).await })
            .await
    }

    /// `resolveRepositoryPaths(cwd, refresh)`.
    pub async fn resolve_repository_paths(&self, cwd: &str, refresh: bool) -> Result<Option<GitRepositoryPaths>, GitCommandError> {
        let key = normalize_cwd_key(cwd);
        if !refresh {
            return self.repository_paths_cached(key).await;
        }
        let this = self.clone();
        let inner_key = key.clone();
        self.inner
            .repository_paths_refresh
            .get(key, move || async move {
                this.inner.repository_paths.invalidate(&inner_key);
                this.repository_paths_cached(inner_key).await
            })
            .await
    }

    async fn cached_default_branch(&self, git_common_dir: &str) -> Result<Option<String>, GitCommandError> {
        let this = self.clone();
        let dir = git_common_dir.to_owned();
        self.inner
            .default_branch
            .get(dir.clone(), move || async move {
                let result = this
                    .git(
                        "GitVcsDriver.statusDetails.defaultBranch",
                        &fetch_cwd_for(&dir),
                        vec!["--git-dir".into(), dir.clone(), "symbolic-ref".into(), "refs/remotes/origin/HEAD".into()],
                        GitOpts::allow_non_zero(),
                    )
                    .await?;
                Ok(if result.exit_code != 0 {
                    None
                } else {
                    parse_default_branch_from_remote_head_ref(&result.stdout, "origin")
                })
            })
            .await
    }

    async fn cached_origin_exists(&self, git_common_dir: &str) -> Result<bool, GitCommandError> {
        let this = self.clone();
        let dir = git_common_dir.to_owned();
        self.inner
            .origin_exists
            .get(dir.clone(), move || async move {
                let result = this
                    .git(
                        "GitVcsDriver.statusDetails.originExists",
                        &fetch_cwd_for(&dir),
                        vec!["--git-dir".into(), dir.clone(), "remote".into(), "get-url".into(), "origin".into()],
                        GitOpts::allow_non_zero(),
                    )
                    .await?;
                Ok(result.exit_code == 0)
            })
            .await
    }

    async fn invalidate_status_static_caches(&self, cwd: &str) {
        let paths = self.resolve_repository_paths(cwd, false).await.ok().flatten();
        let key = paths.map(|p| p.git_common_dir).unwrap_or_else(|| normalize_cwd_key(cwd));
        self.inner.default_branch.invalidate(&key);
        self.inner.origin_exists.invalidate(&key);
    }

    async fn resolve_git_common_dir(&self, cwd: &str) -> Result<String, GitCommandError> {
        match self.resolve_repository_paths(cwd, false).await? {
            Some(paths) => Ok(paths.git_common_dir),
            None => Err(GitCommandError::git(
                "GitVcsDriver.resolveGitCommonDir",
                cwd,
                2,
                "Cannot resolve a Git common directory outside a repository.",
            )),
        }
    }

    async fn refresh_status_upstream_if_stale(&self, cwd: &str) -> Result<(), GitCommandError> {
        let Some(upstream) = self.resolve_current_upstream(cwd).await? else {
            return Ok(());
        };
        let git_common_dir = self.resolve_git_common_dir(cwd).await?;
        let this = self.clone();
        let key = (git_common_dir.clone(), upstream.remote_name.clone());
        let failures_key = failure_key(&git_common_dir, &upstream.remote_name);
        let _ = self
            .inner
            .remote_refresh
            .get(key, move || async move {
                match this.fetch_remote_for_status(&git_common_dir, &upstream.remote_name).await {
                    Ok(()) => {
                        this.inner
                            .remote_refresh_failures
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&failures_key);
                        Ok(true)
                    }
                    Err(error) => {
                        {
                            let mut failures = this.inner.remote_refresh_failures.lock().unwrap_or_else(|p| p.into_inner());
                            let next = failures.get(&failures_key).unwrap_or(0) + 1;
                            failures.set(&failures_key, next);
                        }
                        tracing::warn!(
                            operation = %error.operation,
                            detail = %error.detail,
                            "Background Git fetch failed"
                        );
                        Err(error)
                    }
                }
            })
            .await;
        Ok(())
    }

    // -----------------------------------------------------------------------------------------
    // Remotes and branches
    // -----------------------------------------------------------------------------------------

    /// `resolveDefaultBranchName(cwd, remoteName)`.
    pub async fn resolve_default_branch_name(&self, cwd: &str, remote_name: &str) -> Result<Option<String>, GitCommandError> {
        let result = self
            .git(
                "GitVcsDriver.resolveDefaultBranchName",
                cwd,
                vec!["symbolic-ref".into(), format!("refs/remotes/{remote_name}/HEAD")],
                GitOpts::allow_non_zero(),
            )
            .await?;
        Ok(if result.exit_code != 0 {
            None
        } else {
            parse_default_branch_from_remote_head_ref(&result.stdout, remote_name)
        })
    }

    /// `remoteBranchExists({cwd, remoteName, refName})`.
    pub async fn remote_branch_exists(&self, cwd: &str, remote_name: &str, ref_name: &str) -> Result<bool, GitCommandError> {
        let result = self
            .git(
                "GitVcsDriver.remoteBranchExists",
                cwd,
                vec![
                    "show-ref".into(),
                    "--verify".into(),
                    "--quiet".into(),
                    format!("refs/remotes/{remote_name}/{ref_name}"),
                ],
                GitOpts::allow_non_zero(),
            )
            .await?;
        Ok(result.exit_code == 0)
    }

    /// `remoteExists({cwd, remoteName})`.
    pub async fn remote_exists(&self, cwd: &str, remote_name: &str) -> Result<bool, GitCommandError> {
        let result = self
            .git(
                "GitVcsDriver.remoteExists",
                cwd,
                vec!["remote".into(), "get-url".into(), remote_name.into()],
                GitOpts::allow_non_zero(),
            )
            .await?;
        Ok(result.exit_code == 0)
    }

    async fn list_remote_names(&self, cwd: &str) -> Result<Vec<String>, GitCommandError> {
        Ok(parse_remote_names_in_git_order(
            &self.stdout("GitVcsDriver.listRemoteNames", cwd, s(&["remote"]), false).await?,
        ))
    }

    async fn resolve_publish_branch_name(&self, cwd: &str, branch: &str) -> String {
        let names = self.list_remote_names(cwd).await.unwrap_or_default();
        parse_remote_ref_with_remote_names(branch, &names)
            .map(|parsed| parsed.branch_name)
            .unwrap_or_else(|| branch.to_owned())
    }

    /// `resolvePrimaryRemoteName`: `origin`, else the first remote.
    pub async fn resolve_primary_remote_name(&self, cwd: &str) -> Result<String, GitCommandError> {
        if self.remote_exists(cwd, "origin").await? {
            return Ok("origin".into());
        }
        let remotes = self.list_remote_names(cwd).await?;
        match remotes.into_iter().next() {
            Some(first) => Ok(first),
            None => Err(GitCommandError::git(
                "GitVcsDriver.resolvePrimaryRemoteName",
                cwd,
                1,
                "No git remote is configured for this repository.",
            )),
        }
    }

    async fn resolve_push_remote_name(&self, cwd: &str, ref_name: &str) -> Result<Option<String>, GitCommandError> {
        let branch_push_remote = self
            .stdout(
                "GitVcsDriver.resolvePushRemoteName.branchPushRemote",
                cwd,
                vec!["config".into(), "--get".into(), format!("branch.{ref_name}.pushRemote")],
                true,
            )
            .await?
            .trim()
            .to_owned();
        if !branch_push_remote.is_empty() {
            return Ok(Some(branch_push_remote));
        }
        let push_default = self
            .stdout(
                "GitVcsDriver.resolvePushRemoteName.remotePushDefault",
                cwd,
                s(&["config", "--get", "remote.pushDefault"]),
                true,
            )
            .await?
            .trim()
            .to_owned();
        if !push_default.is_empty() {
            return Ok(Some(push_default));
        }
        Ok(self.resolve_primary_remote_name(cwd).await.ok())
    }

    async fn ensure_remote_inner(&self, cwd: &str, preferred_name: &str, url: &str) -> Result<String, GitCommandError> {
        let preferred = sanitize_remote_name(preferred_name);
        let target = normalize_git_remote_url(url);
        let remotes = parse_remote_fetch_urls(
            &self
                .stdout("GitVcsDriver.ensureRemote.listRemoteUrls", cwd, s(&["remote", "-v"]), false)
                .await?,
        );
        for (name, remote_url) in &remotes {
            if normalize_git_remote_url(remote_url) == target {
                return Ok(name.clone());
            }
        }
        let mut name = preferred.clone();
        let mut suffix = 1;
        while remotes.iter().any(|(existing, _)| *existing == name) {
            name = format!("{preferred}-{suffix}");
            suffix += 1;
        }
        self.run_git(
            "GitVcsDriver.ensureRemote.add",
            cwd,
            vec!["remote".into(), "add".into(), name.clone(), url.into()],
            GitOpts::default(),
        )
        .await?;
        Ok(name)
    }

    /// `resolveBaseBranchForNoUpstream`.
    pub async fn resolve_base_branch_for_no_upstream(&self, cwd: &str, ref_name: &str) -> Result<Option<String>, GitCommandError> {
        let configured = self
            .stdout(
                "GitVcsDriver.resolveBaseBranchForNoUpstream.config",
                cwd,
                vec!["config".into(), "--get".into(), format!("branch.{ref_name}.gh-merge-base")],
                true,
            )
            .await?
            .trim()
            .to_owned();
        let primary = self.resolve_primary_remote_name(cwd).await.ok();
        let default_branch = match &primary {
            None => None,
            Some(remote) => self.resolve_default_branch_name(cwd, remote).await?,
        };
        let mut candidates: Vec<Option<String>> = vec![(!configured.is_empty()).then_some(configured), default_branch];
        candidates.extend(DEFAULT_BASE_BRANCH_CANDIDATES.iter().map(|c| Some((*c).to_owned())));
        for candidate in candidates.into_iter().flatten() {
            if candidate.is_empty() {
                continue;
            }
            let remote_prefix = primary.as_deref().filter(|p| *p != "origin").map(|p| format!("{p}/"));
            let normalized = if let Some(rest) = candidate.strip_prefix("origin/") {
                rest.to_owned()
            } else if let Some(rest) = remote_prefix.as_deref().and_then(|prefix| candidate.strip_prefix(prefix)) {
                rest.to_owned()
            } else {
                candidate.clone()
            };
            if normalized.is_empty() || normalized == ref_name {
                continue;
            }
            if let Some(primary) = &primary {
                if self.remote_branch_exists(cwd, primary, &normalized).await? {
                    return Ok(Some(format!("{primary}/{normalized}")));
                }
            }
            if self.branch_exists(cwd, &normalized).await? {
                return Ok(Some(normalized));
            }
        }
        Ok(None)
    }

    async fn compute_ahead_count_against_base(&self, cwd: &str, ref_name: &str) -> Result<u64, GitCommandError> {
        let Some(base) = self.resolve_base_branch_for_no_upstream(cwd, ref_name).await? else {
            return Ok(0);
        };
        let result = self
            .git(
                "GitVcsDriver.computeAheadCountAgainstBase",
                cwd,
                vec!["rev-list".into(), "--count".into(), format!("{base}..HEAD")],
                GitOpts::allow_non_zero(),
            )
            .await?;
        if result.exit_code != 0 {
            return Ok(0);
        }
        Ok(js_parse_int(result.stdout.trim()).map(|n| n.max(0) as u64).unwrap_or(0))
    }

    // -----------------------------------------------------------------------------------------
    // Status
    // -----------------------------------------------------------------------------------------

    async fn read_status_details_remote(&self, cwd: &str) -> Result<GitRemoteStatusDetails, GitCommandError> {
        let operation = "GitVcsDriver.statusDetailsRemote.branch";
        let branch_result = match self
            .git_c(operation, cwd, s(&["rev-parse", "--abbrev-ref", "HEAD"]), GitOpts::allow_non_zero())
            .await
        {
            Ok(result) => result,
            Err(error) if error.missing_cwd => return Ok(GitRemoteStatusDetails::non_repository()),
            Err(error) => return Err(error),
        };
        let branch = if branch_result.exit_code != 0 {
            if is_non_repository_git_stderr(&branch_result.stderr) {
                return Ok(GitRemoteStatusDetails::non_repository());
            }
            if !is_unborn_head_stderr(&branch_result.stderr) {
                return Err(GitCommandError {
                    exit_code: Some(branch_result.exit_code),
                    stdout_length: Some(js_length(&branch_result.stdout)),
                    stderr_length: Some(js_length(&branch_result.stderr)),
                    ..GitCommandError::git(operation, cwd, 3, "Git branch lookup failed.")
                });
            }
            let value = self
                .stdout(
                    "GitVcsDriver.statusDetailsRemote.unbornBranch",
                    cwd,
                    s(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
                    false,
                )
                .await?;
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_owned())
        } else {
            let value = branch_result.stdout.trim();
            (!value.is_empty() && value != "HEAD").then(|| value.to_owned())
        };
        let upstream = self.resolve_current_upstream(cwd).await?;
        let upstream_ref = upstream.map(|u| u.upstream_ref);
        let mut ahead = 0;
        let mut behind = 0;
        if let Some(upstream_ref) = &upstream_ref {
            let divergence = self
                .git(
                    "GitVcsDriver.statusDetailsRemote.divergence",
                    cwd,
                    vec!["rev-list".into(), "--left-right".into(), "--count".into(), format!("HEAD...{upstream_ref}")],
                    GitOpts::allow_non_zero(),
                )
                .await?;
            if divergence.exit_code == 0 {
                let mut parts = divergence.stdout.split_whitespace();
                ahead = js_parse_int(parts.next().unwrap_or("0")).map(|v| v.max(0) as u64).unwrap_or(0);
                behind = js_parse_int(parts.next().unwrap_or("0")).map(|v| v.max(0) as u64).unwrap_or(0);
            }
        } else if let Some(branch) = &branch {
            ahead = self.compute_ahead_count_against_base(cwd, branch).await.unwrap_or(0);
        }
        let default_branch = self.resolve_default_branch_name(cwd, "origin").await?;
        let is_default_branch = branch
            .as_deref()
            .is_some_and(|b| Some(b) == default_branch.as_deref() || (default_branch.is_none() && (b == "main" || b == "master")));
        let ahead_of_default = match &branch {
            Some(b) if !is_default_branch => {
                if upstream_ref.is_none() {
                    ahead
                } else {
                    self.compute_ahead_count_against_base(cwd, b).await.unwrap_or(0)
                }
            }
            _ => 0,
        };
        Ok(GitRemoteStatusDetails {
            is_repo: true,
            default_branch,
            is_default_branch,
            branch,
            has_upstream: upstream_ref.is_some(),
            upstream_ref,
            ahead_count: ahead,
            behind_count: behind,
            ahead_of_default_count: ahead_of_default,
        })
    }

    async fn read_status_details_local(&self, cwd: &str) -> Result<GitStatusDetails, GitCommandError> {
        let index_operation = "GitVcsDriver.statusDetails.indexPath";
        let index_result = match self
            .git_c(index_operation, cwd, s(&["rev-parse", "--git-path", "index"]), GitOpts::allow_non_zero())
            .await
        {
            Ok(result) => result,
            Err(error) if error.missing_cwd => return Ok(GitStatusDetails::non_repository()),
            Err(error) => return Err(error),
        };
        if index_result.exit_code == 0 {
            let lock_path = format!("{}.lock", resolve_from(cwd, index_result.stdout.trim()));
            // Status can succeed while locked, repeatedly running LFS clean filters uncached.
            match tokio::fs::try_exists(&lock_path).await {
                Ok(true) => {
                    return Err(GitCommandError::new(
                        index_operation,
                        "git",
                        cwd,
                        "Git index is locked. Status will resume when the index lock is removed.",
                    ))
                }
                Ok(false) => {}
                Err(io) => {
                    return Err(GitCommandError::new(index_operation, "git", cwd, "Failed to check the Git index lock.")
                        .with_cause(platform_error_defect("exists", &lock_path, &io)))
                }
            }
        }
        let status_operation = "GitVcsDriver.statusDetails.status";
        let status = match self
            .git_c(status_operation, cwd, s(&["status", "--porcelain=2", "--branch"]), GitOpts::allow_non_zero())
            .await
        {
            Ok(result) => result,
            Err(error) if error.missing_cwd => return Ok(GitStatusDetails::non_repository()),
            Err(error) => return Err(error),
        };
        if status.exit_code != 0 {
            if is_non_repository_git_stderr(&status.stderr) {
                return Ok(GitStatusDetails::non_repository());
            }
            return Err(GitCommandError {
                exit_code: Some(status.exit_code),
                stdout_length: Some(js_length(&status.stdout)),
                stderr_length: Some(js_length(&status.stderr)),
                ..GitCommandError::git(status_operation, cwd, 3, "Git status failed.")
            });
        }
        let status_cache_key = self.resolve_repository_paths(cwd, false).await.ok().flatten().map(|p| p.git_common_dir);

        let numstat = async {
            let result = self
                .git_c(
                    "GitVcsDriver.statusDetails.numstat",
                    cwd,
                    s(&["diff", "HEAD", "--numstat", "--"]),
                    GitOpts::allow_non_zero(),
                )
                .await?;
            if result.exit_code == 0 {
                return Ok(result.stdout);
            }
            if is_unborn_head_stderr(&result.stderr) {
                let (unstaged, staged) = tokio::try_join!(
                    self.stdout("GitVcsDriver.statusDetails.numstat.unborn", cwd, s(&["diff", "--numstat"]), false),
                    self.stdout(
                        "GitVcsDriver.statusDetails.numstat.unborn.staged",
                        cwd,
                        s(&["diff", "--cached", "--numstat"]),
                        false
                    )
                )?;
                return Ok(merge_unborn_numstat(&unstaged, &staged));
            }
            Err(GitCommandError {
                exit_code: Some(result.exit_code),
                stdout_length: Some(js_length(&result.stdout)),
                stderr_length: Some(js_length(&result.stderr)),
                ..GitCommandError::git("GitVcsDriver.statusDetails.numstat", cwd, 4, "git diff HEAD --numstat failed.")
            })
        };
        let default_branch = async {
            match &status_cache_key {
                Some(key) => self.cached_default_branch(key).await.unwrap_or(None),
                None => self.resolve_default_branch_name(cwd, "origin").await.unwrap_or(None),
            }
        };
        let has_primary_remote = async {
            match &status_cache_key {
                Some(key) => self.cached_origin_exists(key).await.unwrap_or(false),
                None => self.remote_exists(cwd, "origin").await.unwrap_or(false),
            }
        };
        let (numstat_stdout, default_branch, has_primary_remote) = tokio::join!(numstat, default_branch, has_primary_remote);
        let numstat_stdout = numstat_stdout?;

        let mut ref_name: Option<String> = None;
        let mut upstream_ref: Option<String> = None;
        let mut ahead = 0;
        let mut behind = 0;
        let mut has_changes = false;
        let mut changed_without_numstat: Vec<String> = Vec::new();
        for line in status.stdout.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(value) = line.strip_prefix("# branch.head ") {
                let value = value.trim();
                ref_name = (!value.starts_with('(')).then(|| value.to_owned());
                continue;
            }
            if let Some(value) = line.strip_prefix("# branch.upstream ") {
                let value = value.trim();
                upstream_ref = (!value.is_empty()).then(|| value.to_owned());
                continue;
            }
            if let Some(value) = line.strip_prefix("# branch.ab ") {
                (ahead, behind) = parse_branch_ab(value.trim());
                continue;
            }
            if !line.trim().is_empty() && !line.starts_with('#') {
                has_changes = true;
                if let Some(path) = parse_porcelain_path(line) {
                    if !changed_without_numstat.contains(&path) {
                        changed_without_numstat.push(path);
                    }
                }
            }
        }

        let fallback_ahead = match (&upstream_ref, &ref_name) {
            (None, Some(name)) => Some(self.compute_ahead_count_against_base(cwd, name).await.unwrap_or(0)),
            _ => None,
        };
        if let Some(fallback) = fallback_ahead {
            ahead = fallback;
            behind = 0;
        }
        let is_default_branch = ref_name
            .as_deref()
            .is_some_and(|name| Some(name) == default_branch.as_deref() || (default_branch.is_none() && (name == "main" || name == "master")));
        let mut ahead_of_default = 0;
        if let Some(name) = &ref_name {
            if !is_default_branch {
                ahead_of_default = match fallback_ahead {
                    Some(fallback) => fallback,
                    None => self.compute_ahead_count_against_base(cwd, name).await.unwrap_or(0),
                };
            }
        }

        // `new Map()` keyed by path: a later entry updates the value in place.
        let mut stats: Vec<WorkingTreeFile> = Vec::new();
        for entry in parse_numstat_entries(&numstat_stdout) {
            match stats.iter_mut().find(|f| f.path == entry.path) {
                Some(existing) => {
                    existing.insertions = entry.insertions;
                    existing.deletions = entry.deletions;
                }
                None => stats.push(WorkingTreeFile {
                    path: entry.path,
                    insertions: entry.insertions,
                    deletions: entry.deletions,
                }),
            }
        }
        let insertions = stats.iter().map(|f| f.insertions).sum();
        let deletions = stats.iter().map(|f| f.deletions).sum();
        let numstat_paths: std::collections::HashSet<String> = stats.iter().map(|f| f.path.clone()).collect();
        let mut files = stats;
        files.sort_by(|a, b| locale_compare(&a.path, &b.path));
        for path in changed_without_numstat {
            if !numstat_paths.contains(&path) {
                files.push(WorkingTreeFile {
                    path,
                    insertions: 0,
                    deletions: 0,
                });
            }
        }
        files.sort_by(|a, b| locale_compare(&a.path, &b.path));

        Ok(GitStatusDetails {
            is_repo: true,
            has_origin_remote: has_primary_remote,
            is_default_branch,
            branch: ref_name,
            has_upstream: upstream_ref.is_some(),
            upstream_ref,
            has_working_tree_changes: has_changes,
            working_tree: WorkingTree { files, insertions, deletions },
            ahead_count: ahead,
            behind_count: behind,
            ahead_of_default_count: ahead_of_default,
        })
    }

    /// `statusDetailsLocal(cwd)`: no background fetch.
    pub async fn status_details_local(&self, cwd: &str) -> Result<GitStatusDetails, GitCommandError> {
        self.read_status_details_local(cwd).await
    }

    /// `statusDetails(cwd)`: refresh the upstream when stale (failures ignored), then read.
    pub async fn status_details(&self, cwd: &str) -> Result<GitStatusDetails, GitCommandError> {
        if let Err(error) = self.refresh_status_upstream_if_stale(cwd).await {
            if !error.missing_cwd {
                tracing::debug!(detail = %error.detail, "status upstream refresh failed");
            }
        }
        self.read_status_details_local(cwd).await
    }

    /// `statusDetailsRemote(cwd, {refreshUpstream})`.
    pub async fn status_details_remote(&self, cwd: &str, refresh_upstream: bool) -> Result<GitRemoteStatusDetails, GitCommandError> {
        if refresh_upstream {
            if let Err(error) = self.refresh_status_upstream_if_stale(cwd).await {
                if !error.missing_cwd {
                    tracing::debug!(detail = %error.detail, "status upstream refresh failed");
                }
            }
        }
        self.read_status_details_remote(cwd).await
    }

    /// `status(input)`: the driver-level status (no forge, `pr: null`).
    pub async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitCommandError> {
        let details = self.status_details(cwd).await?;
        Ok(VcsStatusResult {
            local: VcsStatusLocalResult {
                is_repo: details.is_repo,
                source_control_provider: None,
                has_primary_remote: details.has_origin_remote,
                is_default_ref: details.is_default_branch,
                ref_name: details.branch,
                has_working_tree_changes: details.has_working_tree_changes,
                working_tree: details.working_tree,
            },
            remote: VcsStatusRemoteResult {
                has_upstream: details.has_upstream,
                ahead_count: details.ahead_count,
                behind_count: details.behind_count,
                ahead_of_default_count: Some(details.ahead_of_default_count),
                pr: None,
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // Commit, push, pull, range context
    // -----------------------------------------------------------------------------------------

    /// `prepareCommitContext(cwd, filePaths?)`: stage (selected files literally, or all) and
    /// summarize. `None` when nothing is staged.
    pub async fn prepare_commit_context(&self, cwd: &str, file_paths: Option<&[String]>) -> Result<Option<GitPreparedCommitContext>, GitCommandError> {
        match file_paths.filter(|paths| !paths.is_empty()) {
            Some(paths) => {
                let _ = self
                    .run_git("GitVcsDriver.prepareCommitContext.reset", cwd, s(&["reset"]), GitOpts::default())
                    .await;
                let mut args = s(&["--literal-pathspecs", "add", "-A", "--"]);
                args.extend(paths.iter().cloned());
                self.run_git("GitVcsDriver.prepareCommitContext.addSelected", cwd, args, GitOpts::default())
                    .await?;
            }
            None => {
                self.run_git("GitVcsDriver.prepareCommitContext.addAll", cwd, s(&["add", "-A"]), GitOpts::default())
                    .await?;
            }
        }
        let staged_summary = self
            .stdout(
                "GitVcsDriver.prepareCommitContext.stagedSummary",
                cwd,
                s(&["diff", "--cached", "--name-status"]),
                false,
            )
            .await?
            .trim()
            .to_owned();
        if staged_summary.is_empty() {
            return Ok(None);
        }
        let staged_patch = self
            .stdout_with(
                "GitVcsDriver.prepareCommitContext.stagedPatch",
                cwd,
                s(&["diff", "--no-ext-diff", "--cached", "--patch", "--minimal"]),
                GitOpts::default().max_bytes(PREPARED_COMMIT_PATCH_MAX_OUTPUT_BYTES).truncate(),
            )
            .await?;
        Ok(Some(GitPreparedCommitContext { staged_summary, staged_patch }))
    }

    async fn commit_inner(&self, cwd: &str, subject: &str, body: &str, options: GitCommitOptions) -> Result<String, GitCommandError> {
        let mut args = vec!["commit".to_owned(), "-m".to_owned(), subject.to_owned()];
        let trimmed_body = body.trim();
        if !trimmed_body.is_empty() {
            args.push("-m".into());
            args.push(trimmed_body.to_owned());
        }
        let progress = options.progress.map(|progress| {
            let (stdout, stderr): (Option<LineCallback>, Option<LineCallback>) = match progress.on_output_line.clone() {
                None => (None, None),
                Some(on_line) => {
                    let out = on_line.clone();
                    (
                        Some(Arc::new(move |line: &str| out(OutputStream::Stdout, line))),
                        Some(Arc::new(move |line: &str| on_line(OutputStream::Stderr, line))),
                    )
                }
            };
            ExecuteGitProgress {
                on_stdout_line: stdout,
                on_stderr_line: stderr,
                on_hook_started: progress.on_hook_started,
                on_hook_finished: progress.on_hook_finished,
            }
        });
        self.run_git(
            "GitVcsDriver.commit.commit",
            cwd,
            args,
            GitOpts {
                timeout: options.timeout_ms.map(GitTimeout::Millis).unwrap_or_default(),
                progress,
                ..GitOpts::default()
            },
        )
        .await?;
        Ok(self
            .stdout("GitVcsDriver.commit.revParseHead", cwd, s(&["rev-parse", "HEAD"]), false)
            .await?
            .trim()
            .to_owned())
    }

    /// `commit(cwd, subject, body, options?)`: returns the new commit sha.
    pub async fn commit(&self, cwd: &str, subject: &str, body: &str, options: GitCommitOptions) -> Result<String, GitCommandError> {
        self.with_invalidation(cwd, self.commit_inner(cwd, subject, body, options)).await
    }

    async fn push_inner(&self, cwd: &str, fallback_branch: Option<&str>, remote_name: Option<&str>) -> Result<GitPushResult, GitCommandError> {
        let details = self.status_details(cwd).await?;
        let Some(branch) = details.branch.clone().or_else(|| fallback_branch.map(str::to_owned)).filter(|b| !b.is_empty()) else {
            return Err(GitCommandError::git(
                "GitVcsDriver.pushCurrentBranch",
                cwd,
                1,
                "Cannot push from detached HEAD.",
            ));
        };
        let pushed = |upstream_branch: String, set_upstream: bool| GitPushResult {
            status: GitPushStatus::Pushed,
            branch: branch.clone(),
            upstream_branch: Some(upstream_branch),
            set_upstream: Some(set_upstream),
        };
        let push = |op: &'static str, args: Vec<String>| async move {
            self.run_git(
                op,
                cwd,
                args,
                GitOpts {
                    timeout: GitTimeout::Unbounded,
                    ..GitOpts::default()
                },
            )
            .await
        };

        if let Some(requested) = remote_name.map(str::trim).filter(|r| !r.is_empty()) {
            let publish = self.resolve_publish_branch_name(cwd, &branch).await;
            push(
                "GitVcsDriver.pushCurrentBranch.pushWithRequestedRemote",
                vec!["push".into(), "-u".into(), requested.into(), format!("HEAD:refs/heads/{publish}")],
            )
            .await?;
            return Ok(pushed(format!("{requested}/{publish}"), true));
        }

        if details.ahead_count == 0 && details.behind_count == 0 {
            if details.has_upstream {
                return Ok(GitPushResult {
                    status: GitPushStatus::SkippedUpToDate,
                    branch: branch.clone(),
                    upstream_branch: details.upstream_ref.clone(),
                    set_upstream: None,
                });
            }
            let comparable = self.resolve_base_branch_for_no_upstream(cwd, &branch).await.ok().flatten();
            if comparable.is_some() {
                let publish_remote = self.resolve_push_remote_name(cwd, &branch).await.ok().flatten();
                let Some(publish_remote) = publish_remote else {
                    return Ok(GitPushResult {
                        status: GitPushStatus::SkippedUpToDate,
                        branch: branch.clone(),
                        upstream_branch: None,
                        set_upstream: None,
                    });
                };
                if self.remote_branch_exists(cwd, &publish_remote, &branch).await.unwrap_or(false) {
                    return Ok(GitPushResult {
                        status: GitPushStatus::SkippedUpToDate,
                        branch: branch.clone(),
                        upstream_branch: None,
                        set_upstream: None,
                    });
                }
            }
        }

        if !details.has_upstream {
            let Some(publish_remote) = self.resolve_push_remote_name(cwd, &branch).await? else {
                return Err(GitCommandError::git(
                    "GitVcsDriver.pushCurrentBranch",
                    cwd,
                    1,
                    "Cannot push because no git remote is configured for this repository.",
                ));
            };
            let publish = self.resolve_publish_branch_name(cwd, &branch).await;
            push(
                "GitVcsDriver.pushCurrentBranch.pushWithUpstream",
                vec!["push".into(), "-u".into(), publish_remote.clone(), format!("HEAD:refs/heads/{publish}")],
            )
            .await?;
            return Ok(pushed(format!("{publish_remote}/{publish}"), true));
        }

        let current_upstream = self.resolve_current_upstream(cwd).await.ok().flatten();
        if let Some(upstream) = current_upstream {
            // A branch tracking a differently named ref was cut from it: that upstream is its
            // base, not its publish target. The one legit difference is a git-mangled alias
            // (`upstream/effect-atom` tracking `my-org/upstream`'s `effect-atom`).
            let is_alias = branch == upstream.branch_name
                || (branch.ends_with(&format!("/{}", upstream.branch_name)) && upstream.upstream_ref.ends_with(&format!("/{branch}")));
            if !is_alias {
                let publish_remote = self.resolve_push_remote_name(cwd, &branch).await.ok().flatten();
                let remote = publish_remote.unwrap_or_else(|| upstream.remote_name.clone());
                let publish = self.resolve_publish_branch_name(cwd, &branch).await;
                let configured_merge_base = self
                    .stdout(
                        "GitVcsDriver.pushCurrentBranch.readMergeBase",
                        cwd,
                        vec!["config".into(), "--get".into(), format!("branch.{branch}.gh-merge-base")],
                        true,
                    )
                    .await?
                    .trim()
                    .to_owned();
                if configured_merge_base.is_empty() {
                    self.run_git(
                        "GitVcsDriver.pushCurrentBranch.recordMergeBase",
                        cwd,
                        vec!["config".into(), format!("branch.{branch}.gh-merge-base"), upstream.branch_name.clone()],
                        GitOpts::default(),
                    )
                    .await?;
                }
                push(
                    "GitVcsDriver.pushCurrentBranch.pushOwnBranch",
                    vec!["push".into(), "-u".into(), remote.clone(), format!("HEAD:refs/heads/{publish}")],
                )
                .await?;
                return Ok(pushed(format!("{remote}/{publish}"), true));
            }
            push(
                "GitVcsDriver.pushCurrentBranch.pushUpstream",
                vec!["push".into(), upstream.remote_name.clone(), format!("HEAD:refs/heads/{}", upstream.branch_name)],
            )
            .await?;
            return Ok(pushed(upstream.upstream_ref.clone(), false));
        }

        push("GitVcsDriver.pushCurrentBranch.push", s(&["push"])).await?;
        Ok(GitPushResult {
            status: GitPushStatus::Pushed,
            branch: branch.clone(),
            upstream_branch: details.upstream_ref.clone(),
            set_upstream: Some(false),
        })
    }

    /// `pushCurrentBranch(cwd, fallbackBranch, {remoteName?})`.
    pub async fn push_current_branch(&self, cwd: &str, fallback_branch: Option<&str>, remote_name: Option<&str>) -> Result<GitPushResult, GitCommandError> {
        self.with_invalidation(cwd, self.push_inner(cwd, fallback_branch, remote_name)).await
    }

    async fn pull_inner(&self, cwd: &str) -> Result<VcsPullResult, GitCommandError> {
        let details = self.status_details(cwd).await?;
        let Some(ref_name) = details.branch.clone() else {
            return Err(GitCommandError::git(
                "GitVcsDriver.pullCurrentBranch",
                cwd,
                2,
                "Cannot pull from detached HEAD.",
            ));
        };
        if !details.has_upstream {
            return Err(GitCommandError::git(
                "GitVcsDriver.pullCurrentBranch",
                cwd,
                2,
                "Current branch has no upstream configured. Push with upstream first.",
            ));
        }
        let before = self
            .stdout("GitVcsDriver.pullCurrentBranch.beforeSha", cwd, s(&["rev-parse", "HEAD"]), true)
            .await?
            .trim()
            .to_owned();
        self.run_git(
            "GitVcsDriver.pullCurrentBranch.pull",
            cwd,
            s(&["pull", "--ff-only"]),
            GitOpts::default().timeout_ms(30_000).fallback("git pull failed"),
        )
        .await?;
        let after = self
            .stdout("GitVcsDriver.pullCurrentBranch.afterSha", cwd, s(&["rev-parse", "HEAD"]), true)
            .await?
            .trim()
            .to_owned();
        let refreshed = self.status_details(cwd).await?;
        Ok(VcsPullResult {
            status: if !before.is_empty() && before == after {
                VcsPullStatus::SkippedUpToDate
            } else {
                VcsPullStatus::Pulled
            },
            ref_name,
            upstream_ref: refreshed.upstream_ref,
        })
    }

    /// `pullCurrentBranch(cwd)`: `git pull --ff-only` on a branch with an upstream.
    pub async fn pull_current_branch(&self, cwd: &str) -> Result<VcsPullResult, GitCommandError> {
        self.with_invalidation(cwd, self.pull_inner(cwd)).await
    }

    /// `readRangeContext(cwd, baseRef)`: log, stat and patch of a branch against its base.
    pub async fn read_range_context(&self, cwd: &str, base_ref: &str) -> Result<GitRangeContext, GitCommandError> {
        let commit_range = format!("{base_ref}..HEAD");
        let diff_range = format!("{base_ref}...HEAD");
        let (commit_summary, diff_summary, diff_patch) = tokio::try_join!(
            self.stdout_with(
                "GitVcsDriver.readRangeContext.log",
                cwd,
                vec!["log".into(), "--oneline".into(), commit_range.clone()],
                GitOpts::default().max_bytes(RANGE_COMMIT_SUMMARY_MAX_OUTPUT_BYTES).truncate(),
            ),
            self.stdout_with(
                "GitVcsDriver.readRangeContext.diffStat",
                cwd,
                vec!["diff".into(), "--stat".into(), diff_range.clone()],
                GitOpts::default().max_bytes(RANGE_DIFF_SUMMARY_MAX_OUTPUT_BYTES).truncate(),
            ),
            self.stdout_with(
                "GitVcsDriver.readRangeContext.diffPatch",
                cwd,
                vec!["diff".into(), "--no-ext-diff".into(), "--patch".into(), "--minimal".into(), diff_range.clone(),],
                GitOpts::default().max_bytes(RANGE_DIFF_PATCH_MAX_OUTPUT_BYTES).truncate(),
            )
        )?;
        Ok(GitRangeContext {
            commit_summary,
            diff_summary,
            diff_patch,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Review diffs
    // -----------------------------------------------------------------------------------------

    /// `prepareReviewIndex`: a temporary copy of the index with untracked files added as
    /// intent-to-add, so patch and stats agree on unstaged renames. `None` when there is
    /// nothing to add.
    async fn prepare_review_index(&self, cwd: &str, untracked_paths: &[String]) -> Result<Option<(EnvOverlay, TempFile)>, GitCommandError> {
        let (staged_deletions, index_value) = tokio::try_join!(
            self.stdout_with(
                "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.stagedDeletions",
                cwd,
                s(&["diff", "--cached", "--name-only", "--diff-filter=D", "-z", "HEAD", "--"]),
                GitOpts::allow_non_zero().max_bytes(REVIEW_METADATA_MAX_OUTPUT_BYTES),
            ),
            self.stdout(
                "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.indexPath",
                cwd,
                s(&["rev-parse", "--git-path", "index"]),
                false,
            )
        )?;
        let staged: std::collections::HashSet<&str> = staged_deletions.split('\0').filter(|p| !p.is_empty()).collect();
        let to_add: Vec<&String> = untracked_paths.iter().filter(|p| !staged.contains(p.as_str())).collect();
        if to_add.is_empty() {
            return Ok(None);
        }
        let platform_error =
            |io: std::io::Error| {
                GitCommandError::new("GitVcsDriver.prepareReviewIndex", "git diff", cwd, "Could not prepare the review index.")
                    .with_cause(platform_error_defect("copyFile", &index_value, &io))
            };
        let index_path = resolve_from(cwd, index_value.trim());
        let temp = TempFile::create(&format!("t3code-review-index-{}-", std::process::id())).map_err(platform_error)?;
        let index_meta = std::fs::metadata(&index_path).ok();
        if let Some(meta) = &index_meta {
            std::fs::copy(&index_path, &temp.0).map_err(platform_error)?;
            // Node truncates timestamps to milliseconds; flooring to the second keeps
            // preceding-second files from looking racy.
            let seconds = mtime_millis(meta).map(|ms| (ms / 1000).max(0)).unwrap_or(0);
            set_file_times(&temp.0, seconds).map_err(platform_error)?;
        }
        let mut overlay = EnvOverlay::new();
        overlay.insert("GIT_INDEX_FILE".into(), Some(path_string(&temp.0)));
        let temp_index_config = s(&["-c", "core.splitIndex=false", "-c", "splitIndex.sharedIndexExpire=never"]);
        if index_meta.is_none() {
            self.run_git(
                "GitVcsDriver.review.emptyIndex",
                cwd,
                s(&["read-tree", "--empty"]),
                GitOpts::default().with_env(overlay.clone()),
            )
            .await?;
        }
        let mut args = temp_index_config.clone();
        args.extend(s(&["update-index", "--no-split-index"]));
        self.run_git(
            "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.expandSplitIndex",
            cwd,
            args,
            GitOpts::default().with_env(overlay.clone()),
        )
        .await?;
        let mut args = temp_index_config;
        args.extend(s(&[
            "--literal-pathspecs",
            "add",
            "--intent-to-add",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]));
        let stdin = format!("{}\0", to_add.iter().map(|p| p.as_str()).collect::<Vec<_>>().join("\0"));
        self.run_git(
            "GitVcsDriver.readUnifiedWorkingTreeReviewDiff.addUntracked",
            cwd,
            args,
            GitOpts {
                stdin: Some(stdin),
                ..GitOpts::default().with_env(overlay.clone())
            },
        )
        .await?;
        Ok(Some((overlay, temp)))
    }

    /// `getReviewDiffPreview(input)`: the working-tree and branch-range sources.
    pub async fn get_review_diff_preview(&self, input: &ReviewDiffPreviewInput) -> Result<ReviewDiffPreviewResult, GitCommandError> {
        let path_args: Vec<String> = match &input.file {
            Some(file) => std::iter::once(file.path.clone())
                .chain(file.previous_path.clone())
                .map(|p| format!(":(top,literal){p}"))
                .collect(),
            None => Vec::new(),
        };
        let patch_limit = if input.file.is_some() {
            REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES
        } else {
            REVIEW_DIFF_PATCH_MAX_OUTPUT_BYTES
        };
        let repository = match self.resolve_repository_paths_uncached(&input.cwd).await {
            Ok(repository) => repository,
            Err(error) if error.missing_cwd => None,
            Err(error) => return Err(error),
        };
        let Some(repository) = repository else {
            return Ok(ReviewDiffPreviewResult::empty(&input.cwd));
        };
        let Some(cwd) = repository.worktree_root.clone() else {
            return Ok(ReviewDiffPreviewResult::empty(&input.cwd));
        };
        let cwd = cwd.as_str();
        let branch = repository.current_branch.clone();
        let base_ref = match &input.base_ref {
            Some(base) => Some(base.clone()),
            None => match &branch {
                Some(branch) => self.resolve_base_branch_for_no_upstream(cwd, branch).await.ok().flatten(),
                None => None,
            },
        };
        let mut diff_args = s(&["diff", "--find-renames", "--no-color", "--no-ext-diff", "--no-textconv", "--minimal"]);
        diff_args.extend(PATCH_RENDER_PREFIX_ARGS.iter().map(|a| (*a).to_owned()));
        if input.ignore_whitespace == Some(true) {
            diff_args.push("--ignore-all-space".into());
        }

        struct Tracked {
            stdout: String,
            stdout_truncated: bool,
            files: Option<Vec<ReviewDiffFileStat>>,
        }
        let empty = || Tracked {
            stdout: String::new(),
            stdout_truncated: false,
            files: Some(Vec::new()),
        };

        let read_stats = |reference: String, overlay: Option<EnvOverlay>| {
            let diff_args = diff_args.clone();
            let path_args = path_args.clone();
            async move {
                let mut args = diff_args.clone();
                args.extend(s(&["--numstat", "-z"]));
                let mut full = args.clone();
                full.push(reference.clone());
                full.push("--".into());
                full.extend(path_args.iter().cloned());
                let result = self
                    .git(
                        "GitVcsDriver.getReviewDiffPreview.stat",
                        cwd,
                        full,
                        GitOpts {
                            env: overlay.clone(),
                            ..GitOpts::allow_non_zero().max_bytes(REVIEW_METADATA_MAX_OUTPUT_BYTES)
                        },
                    )
                    .await?;
                if result.exit_code == 0 {
                    return Ok((reference, parse_review_numstat(&result.stdout)));
                }
                if reference == "HEAD" && is_unborn_head_stderr(&result.stderr) {
                    let empty_tree = self
                        .stdout(
                            "GitVcsDriver.getReviewDiffPreview.emptyTree",
                            cwd,
                            s(&["hash-object", "-t", "tree", "/dev/null"]),
                            false,
                        )
                        .await?
                        .trim()
                        .to_owned();
                    let mut full = args.clone();
                    full.push(empty_tree.clone());
                    full.push("--".into());
                    full.extend(path_args.iter().cloned());
                    let stdout = self
                        .stdout_with(
                            "GitVcsDriver.getReviewDiffPreview.unbornStat",
                            cwd,
                            full,
                            GitOpts {
                                env: overlay,
                                ..GitOpts::default().max_bytes(REVIEW_METADATA_MAX_OUTPUT_BYTES)
                            },
                        )
                        .await?;
                    return Ok((empty_tree, parse_review_numstat(&stdout)));
                }
                Err(GitCommandError {
                    exit_code: Some(result.exit_code),
                    ..GitCommandError::new(
                        "GitVcsDriver.getReviewDiffPreview.stat",
                        "git diff --numstat",
                        cwd,
                        "Could not read complete diff statistics.",
                    )
                })
            }
        };
        let read_tracked = |reference: Option<String>, overlay: Option<EnvOverlay>| {
            let diff_args = diff_args.clone();
            let path_args = path_args.clone();
            let read_stats = &read_stats;
            async move {
                let Some(reference) = reference else {
                    return Ok::<Tracked, GitCommandError>(empty());
                };
                let (stat_ref, files) = read_stats(reference, overlay.clone()).await?;
                if files.is_empty() {
                    return Ok(empty());
                }
                let mut args = diff_args.clone();
                args.push("--patch".into());
                args.push(stat_ref);
                args.push("--".into());
                args.extend(path_args.iter().cloned());
                let patch = self
                    .git(
                        "GitVcsDriver.getReviewDiffPreview.patch",
                        cwd,
                        args,
                        GitOpts {
                            env: overlay,
                            ..GitOpts::default().max_bytes(patch_limit).truncate()
                        },
                    )
                    .await?;
                Ok(Tracked {
                    stdout: patch.stdout,
                    stdout_truncated: patch.stdout_truncated,
                    files: Some(files),
                })
            }
        };

        let read_dirty = async {
            if input.file.as_ref().map(|f| f.source_kind) == Some(ReviewDiffPreviewSourceKind::BranchRange) {
                return read_tracked(None, None).await;
            }
            let mut args = s(&["ls-files", "--others", "--exclude-standard", "-z", "--"]);
            args.extend(path_args.iter().cloned());
            let untracked = match self
                .git(
                    "GitVcsDriver.review.listUntracked",
                    cwd,
                    args,
                    GitOpts::default().max_bytes(REVIEW_METADATA_MAX_OUTPUT_BYTES),
                )
                .await
            {
                Ok(result) => Some(result),
                Err(error) if error.output_length.is_none() => None,
                Err(error) => return Err(error),
            };
            let Some(untracked) = untracked else {
                let tracked = read_tracked(Some("HEAD".into()), None).await?;
                return Ok(Tracked {
                    files: None,
                    stdout_truncated: true,
                    ..tracked
                });
            };
            let paths: Vec<String> = split_null_separated_paths(&untracked.stdout, untracked.stdout_truncated)
                .into_iter()
                .filter(|candidate| input.file.as_ref().is_none_or(|file| *candidate == file.path))
                .collect();
            if paths.is_empty() {
                return read_tracked(Some("HEAD".into()), None).await;
            }
            let prepared = self.prepare_review_index(cwd, &paths).await?;
            let (overlay, _temp) = match prepared {
                Some((overlay, temp)) => (Some(overlay), Some(temp)),
                None => (None, None),
            };
            read_tracked(Some("HEAD".into()), overlay).await
        };
        let base_target = match (&base_ref, &branch) {
            (Some(base), Some(_)) if input.file.as_ref().map(|f| f.source_kind) != Some(ReviewDiffPreviewSourceKind::WorkingTree) => {
                Some(format!("{base}...HEAD"))
            }
            _ => None,
        };
        let (dirty, base) = tokio::try_join!(read_dirty, read_tracked(base_target, None))?;

        let hash = |diff: &str, files: &[ReviewDiffFileStat]| {
            let encoded = serde_json::to_string(&(diff, files)).unwrap_or_default();
            sha256_hex(encoded.as_bytes())
        };
        let dirty_hash = hash(&dirty.stdout, dirty.files.as_deref().unwrap_or(&[]));
        let base_files = base.files.unwrap_or_default();
        let base_hash = hash(&base.stdout, &base_files);
        let sources = vec![
            ReviewDiffPreviewSource {
                id: "working-tree".into(),
                kind: ReviewDiffPreviewSourceKind::WorkingTree,
                title: "Dirty worktree".into(),
                base_ref: Some("HEAD".into()),
                head_ref: None,
                diff: dirty.stdout,
                diff_hash: dirty_hash,
                truncated: dirty.stdout_truncated,
                files: dirty.files,
            },
            ReviewDiffPreviewSource {
                id: "branch-range".into(),
                kind: ReviewDiffPreviewSourceKind::BranchRange,
                title: match &base_ref {
                    Some(base) => format!("Against {base}"),
                    None => "Against base branch".into(),
                },
                base_ref: base_ref.clone(),
                head_ref: Some(branch.clone().unwrap_or_else(|| "HEAD".into())),
                diff: base.stdout,
                diff_hash: base_hash,
                truncated: base.stdout_truncated,
                files: Some(base_files),
            },
        ];
        Ok(ReviewDiffPreviewResult {
            cwd: input.cwd.clone(),
            generated_at: zc_core::time::now_iso(),
            sources,
        })
    }

    fn review_file_error(input: &ReviewDiffFileContentsInput, detail: impl Into<String>) -> GitCommandError {
        GitCommandError::new("GitVcsDriver.getReviewDiffFileContents", "git", &input.cwd, detail)
    }

    async fn read_review_file_at_revision(&self, input: &ReviewDiffFileContentsInput, revision: &str, relative_path: &str) -> Result<String, GitCommandError> {
        let result = self
            .git(
                "GitVcsDriver.getReviewDiffFileContents.revision",
                &input.cwd,
                vec!["show".into(), format!("{revision}:{relative_path}")],
                GitOpts::default().max_bytes(REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES),
            )
            .await?;
        if result.stdout.contains('\0') {
            return Err(Self::review_file_error(input, format!("Cannot expand binary file '{relative_path}'.")));
        }
        Ok(result.stdout)
    }

    async fn read_working_tree_review_file(&self, input: &ReviewDiffFileContentsInput, repository_root: &str) -> Result<String, GitCommandError> {
        let file_error = |stage: &str, detail: String, cause: Option<Defect>| GitCommandError {
            cause,
            ..GitCommandError::new(format!("GitVcsDriver.getReviewDiffFileContents.workingTree.{stage}"), stage, &input.cwd, detail)
        };
        let requested = crate::git_exec::resolve_against(repository_root, &input.new_path);
        if !is_path_within_root(Path::new(repository_root), &requested) {
            return Err(file_error(
                "path.resolve",
                format!("Diff file '{}' resolves outside the review workspace.", input.new_path),
                None,
            ));
        }
        let requested_str = path_string(&requested);
        let (real_root, real_target) = match (tokio::fs::canonicalize(repository_root).await, tokio::fs::canonicalize(&requested).await) {
            (Ok(root), Ok(target)) => (root, target),
            (Err(io), _) => {
                return Err(file_error(
                    "fs.realPath",
                    format!("Could not resolve diff file '{}'.", input.new_path),
                    Some(platform_error_defect("realPath", repository_root, &io)),
                ))
            }
            (_, Err(io)) => {
                return Err(file_error(
                    "fs.realPath",
                    format!("Could not resolve diff file '{}'.", input.new_path),
                    Some(platform_error_defect("realPath", &requested_str, &io)),
                ))
            }
        };
        let real_target_str = path_string(&real_target);
        if !is_path_within_root(&real_root, &real_target) {
            return Err(file_error(
                "fs.realPath",
                format!("Diff file '{}' resolves outside the review workspace.", input.new_path),
                None,
            ));
        }
        let meta = tokio::fs::metadata(&real_target).await.map_err(|io| {
            file_error(
                "fs.stat",
                format!("Could not inspect diff file '{}'.", input.new_path),
                Some(platform_error_defect("stat", &real_target_str, &io)),
            )
        })?;
        if !meta.is_file() {
            return Err(file_error("fs.stat", format!("Diff path '{}' is not a file.", input.new_path), None));
        }
        if meta.len() > REVIEW_DIFF_FILE_MAX_OUTPUT_BYTES as u64 {
            return Err(file_error(
                "fs.stat",
                format!("Diff file '{}' exceeds the 1 MB expansion limit.", input.new_path),
                None,
            ));
        }
        let bytes = tokio::fs::read(&real_target).await.map_err(|io| {
            file_error(
                "fs.readFile",
                format!("Could not read diff file '{}'.", input.new_path),
                Some(platform_error_defect("readFile", &real_target_str, &io)),
            )
        })?;
        if bytes.contains(&0) {
            return Err(file_error("fs.readFile", format!("Cannot expand binary file '{}'.", input.new_path), None));
        }
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// `getReviewDiffFileContents(input)`: both sides of one file, for expanding a diff.
    pub async fn get_review_diff_file_contents(&self, input: &ReviewDiffFileContentsInput) -> Result<ReviewDiffFileContentsResult, GitCommandError> {
        if input.source_kind == ReviewDiffPreviewSourceKind::WorkingTree {
            let root = self
                .stdout(
                    "GitVcsDriver.getReviewDiffFileContents.repositoryRoot",
                    &input.cwd,
                    s(&["rev-parse", "--show-toplevel"]),
                    false,
                )
                .await?
                .trim()
                .to_owned();
            if root.is_empty() {
                return Err(Self::review_file_error(input, "Could not resolve the Git repository root."));
            }
            let base = input.base_ref.clone().unwrap_or_else(|| "HEAD".into());
            let (old_contents, new_contents) = tokio::try_join!(
                async {
                    if input.change_type == ReviewDiffChangeType::New {
                        Ok(String::new())
                    } else {
                        self.read_review_file_at_revision(input, &base, &input.old_path).await
                    }
                },
                async {
                    if input.change_type == ReviewDiffChangeType::Deleted {
                        Ok(String::new())
                    } else {
                        self.read_working_tree_review_file(input, &root).await
                    }
                }
            )?;
            return Ok(ReviewDiffFileContentsResult { old_contents, new_contents });
        }
        let (Some(base_ref), Some(head_ref)) = (&input.base_ref, &input.head_ref) else {
            return Err(Self::review_file_error(input, "Branch diff file expansion requires both base and head refs."));
        };
        let merge_base = self
            .stdout(
                "GitVcsDriver.getReviewDiffFileContents.mergeBase",
                &input.cwd,
                vec!["merge-base".into(), base_ref.clone(), head_ref.clone()],
                false,
            )
            .await?
            .trim()
            .to_owned();
        if merge_base.is_empty() {
            return Err(Self::review_file_error(input, "Could not resolve the branch comparison base."));
        }
        let (old_contents, new_contents) = tokio::try_join!(
            async {
                if input.change_type == ReviewDiffChangeType::New {
                    Ok(String::new())
                } else {
                    self.read_review_file_at_revision(input, &merge_base, &input.old_path).await
                }
            },
            async {
                if input.change_type == ReviewDiffChangeType::Deleted {
                    Ok(String::new())
                } else {
                    self.read_review_file_at_revision(input, head_ref, &input.new_path).await
                }
            }
        )?;
        Ok(ReviewDiffFileContentsResult { old_contents, new_contents })
    }

    /// `readConfigValue(cwd, key)`: trimmed, `None` when unset.
    pub async fn read_config_value(&self, cwd: &str, key: &str) -> Result<Option<String>, GitCommandError> {
        let value = self
            .stdout("GitVcsDriver.readConfigValue", cwd, vec!["config".into(), "--get".into(), key.into()], true)
            .await?;
        let trimmed = value.trim();
        Ok((!trimmed.is_empty()).then(|| trimmed.to_owned()))
    }

    // -----------------------------------------------------------------------------------------
    // Refs
    // -----------------------------------------------------------------------------------------

    async fn read_git_refs_snapshot(&self, git_common_dir: &str) -> Result<GitRefsSnapshot, GitCommandError> {
        let fetch_cwd = fetch_cwd_for(git_common_dir);
        let git_dir = |rest: &[&str]| {
            let mut args = vec!["--git-dir".to_owned(), git_common_dir.to_owned()];
            args.extend(rest.iter().map(|a| (*a).to_owned()));
            args
        };
        let (refs, default_ref, worktrees, remote_names) = tokio::try_join!(
            self.git_c(
                "GitVcsDriver.listRefs.snapshotRefs",
                &fetch_cwd,
                git_dir(&[
                    "for-each-ref",
                    "--format=%(refname)%09%(committerdate:unix)%09%(symref)",
                    "refs/heads",
                    "refs/remotes",
                ]),
                GitOpts::default()
                    .timeout_ms(30_000)
                    .max_bytes(16 * 1024 * 1024)
                    .fallback("Git ref snapshot enumeration failed."),
            ),
            self.git(
                "GitVcsDriver.listRefs.defaultRef",
                &fetch_cwd,
                git_dir(&["symbolic-ref", "refs/remotes/origin/HEAD"]),
                GitOpts::allow_non_zero().timeout_ms(5_000),
            ),
            self.git(
                "GitVcsDriver.listRefs.worktreeList",
                &fetch_cwd,
                git_dir(&["worktree", "list", "--porcelain", "-z"]),
                GitOpts::allow_non_zero().timeout_ms(30_000).max_bytes(16 * 1024 * 1024),
            ),
            self.git(
                "GitVcsDriver.listRefs.remoteNames",
                &fetch_cwd,
                git_dir(&["remote"]),
                GitOpts::allow_non_zero().timeout_ms(5_000),
            )
        )?;
        let remote_names = if remote_names.exit_code == 0 {
            parse_remote_names(&remote_names.stdout)
        } else {
            if !remote_names.stderr.trim().is_empty() {
                tracing::warn!(
                    "GitVcsDriver.listRefs: remote name lookup returned code {} for {git_common_dir}. Falling back to an empty remote name list.",
                    remote_names.exit_code
                );
            }
            Vec::new()
        };
        let default_branch = (default_ref.exit_code == 0).then(|| {
            let trimmed = default_ref.stdout.trim();
            trimmed.strip_prefix("refs/remotes/origin/").unwrap_or(trimmed).to_owned()
        });
        let parsed_worktrees: Vec<(String, String)> = if worktrees.exit_code == 0 {
            parse_worktree_branch_paths(&worktrees.stdout)
                .into_iter()
                .map(|(branch, path)| (branch, normalize_cwd_key(&path)))
                .collect()
        } else {
            Vec::new()
        };
        let mut worktree_map: Vec<(String, String)> = Vec::new();
        for (branch, path) in parsed_worktrees {
            if path_exists(&path).await {
                worktree_map.push((branch, path));
            }
        }
        let worktree_for = |name: &str| worktree_map.iter().find(|(branch, _)| branch == name).map(|(_, path)| path.clone());

        let mut local: Vec<(VcsRef, i64)> = Vec::new();
        let mut remote: Vec<(VcsRef, i64)> = Vec::new();
        for line in refs.stdout.split('\n') {
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split('\t');
            let full = parts.next().unwrap_or_default();
            let last_commit_raw = parts.next().unwrap_or("0");
            let symbolic = parts.next().unwrap_or_default();
            if full.is_empty() || !symbolic.is_empty() {
                continue;
            }
            let last_commit = js_parse_int(last_commit_raw).unwrap_or(0);
            if let Some(name) = full.strip_prefix("refs/heads/") {
                local.push((
                    VcsRef {
                        name: name.to_owned(),
                        is_remote: Some(false),
                        remote_name: None,
                        current: false,
                        is_default: Some(name) == default_branch.as_deref(),
                        worktree_path: worktree_for(name),
                    },
                    last_commit,
                ));
                continue;
            }
            let Some(name) = full.strip_prefix("refs/remotes/") else {
                continue;
            };
            let parsed = parse_remote_ref_with_remote_names(name, &remote_names);
            remote.push((
                VcsRef {
                    name: name.to_owned(),
                    is_remote: Some(true),
                    remote_name: parsed.as_ref().map(|p| p.remote_name.clone()),
                    current: false,
                    is_default: default_branch.is_some()
                        && parsed
                            .as_ref()
                            .is_some_and(|p| p.remote_name == "origin" && Some(p.branch_name.as_str()) == default_branch.as_deref()),
                    worktree_path: None,
                },
                last_commit,
            ));
        }
        let by_recency_then_name = |a: &(VcsRef, i64), b: &(VcsRef, i64)| b.1.cmp(&a.1).then_with(|| locale_compare(&a.0.name, &b.0.name));
        local.sort_by(by_recency_then_name);
        remote.sort_by(by_recency_then_name);
        Ok(GitRefsSnapshot {
            local_branches: local.into_iter().map(|(r, _)| r).collect(),
            remote_branches: remote.into_iter().map(|(r, _)| r).collect(),
            has_primary_remote: remote_names.iter().any(|n| n == "origin"),
        })
    }

    fn counters(&self) -> std::sync::MutexGuard<'_, ListRefsCounters> {
        self.inner.list_refs_counters.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn snapshot_by_epoch(&self, git_common_dir: String, epoch: u64) -> BoxFuture<'static, Result<Arc<GitRefsSnapshot>, GitCommandError>> {
        let this = self.clone();
        async move {
            let loader_this = this.clone();
            let dir = git_common_dir.clone();
            this.inner
                .list_refs_snapshots
                .get((git_common_dir, epoch), move || async move {
                    loader_this.read_git_refs_snapshot(&dir).await.map(Arc::new)
                })
                .await
        }
        .boxed()
    }

    async fn resolve_list_refs_snapshot(&self, git_common_dir: &str, refresh: bool) -> Result<Arc<GitRefsSnapshot>, GitCommandError> {
        loop {
            let (generation, current_epoch) = {
                let mut counters = self.counters();
                let generation = counters.current_generation(git_common_dir);
                (generation, counters.epochs().get(git_common_dir))
            };
            let snapshot = match current_epoch {
                Some(epoch) if !refresh => self.snapshot_by_epoch(git_common_dir.to_owned(), epoch).await?,
                _ => {
                    // The refresh cache owns the whole snapshot read, so slow repositories stay
                    // single-flight for the entire scan.
                    let this = self.clone();
                    let dir = git_common_dir.to_owned();
                    self.inner
                        .list_refs_refresh
                        .get((git_common_dir.to_owned(), generation), move || async move {
                            let epoch = this.counters().bump_epoch(&dir);
                            this.snapshot_by_epoch(dir, epoch).await
                        })
                        .await?
                }
            };
            if self.counters().current_generation(git_common_dir) == generation {
                return Ok(snapshot);
            }
        }
    }

    async fn invalidate_list_refs_snapshot(&self, cwd: &str) -> Result<(), GitCommandError> {
        let key = normalize_cwd_key(cwd);
        let Some(paths) = self.repository_paths_cached(key.clone()).await? else {
            return Ok(());
        };
        let dir = paths.git_common_dir;
        let previous_generation = {
            let mut counters = self.counters();
            let previous = counters.current_generation(&dir);
            counters.bump_generation(&dir);
            counters.bump_epoch(&dir);
            previous
        };
        self.inner.list_refs_refresh.invalidate(&(dir, previous_generation));
        self.inner.repository_paths_refresh.invalidate(&key);
        self.inner.repository_paths.invalidate(&key);
        Ok(())
    }

    /// `withListRefsInvalidation`: run, then invalidate the ref snapshot and the status static
    /// caches of `cwd` whatever the outcome.
    async fn with_invalidation<T, F>(&self, cwd: &str, work: F) -> Result<T, GitCommandError>
    where
        F: std::future::Future<Output = Result<T, GitCommandError>>,
    {
        let result = work.await;
        let _ = self.invalidate_list_refs_snapshot(cwd).await;
        self.invalidate_status_static_caches(cwd).await;
        result
    }

    /// `listRefs(input)`.
    pub async fn list_refs(&self, input: &VcsListRefsInput) -> Result<VcsListRefsResult, GitCommandError> {
        let refresh = input.refresh == Some(true);
        let paths = match self.resolve_repository_paths(&input.cwd, refresh).await {
            Ok(paths) => paths,
            Err(error) if error.missing_cwd => None,
            Err(error) => return Err(error),
        };
        let Some(paths) = paths else {
            return Ok(VcsListRefsResult::non_repository());
        };
        let snapshot = self.resolve_list_refs_snapshot(&paths.git_common_dir, refresh).await?;
        let has_current_worktree_branch = paths.worktree_root.is_some() && snapshot.local_branches.iter().any(|r| r.worktree_path == paths.worktree_root);
        let local: Vec<VcsRef> = snapshot
            .local_branches
            .iter()
            .cloned()
            .map(|mut r| {
                r.current = if has_current_worktree_branch {
                    r.worktree_path == paths.worktree_root
                } else {
                    Some(r.name.as_str()) == paths.current_branch.as_deref()
                };
                r
            })
            .collect();
        let mut combined: Vec<VcsRef> = local;
        combined.extend(snapshot.remote_branches.iter().cloned());
        let mut combined = if input.include_matching_remote_refs == Some(true) {
            combined
        } else {
            dedupe_remote_branches_with_local_matches(combined)
        };
        // Keep current/default refs on the first page even when the default only exists as
        // origin/<default>.
        let priority = |r: &VcsRef| {
            if r.current {
                0
            } else if r.is_default {
                1
            } else {
                2
            }
        };
        combined.sort_by_key(priority);
        let by_kind: Vec<VcsRef> = match input.ref_kind {
            Some(VcsRefKind::Local) => combined.into_iter().filter(|r| !r.is_remote()).collect(),
            Some(VcsRefKind::Remote) => combined.into_iter().filter(|r| r.is_remote()).collect(),
            _ => combined,
        };
        let filtered = filter_refs_for_query(by_kind, input.query.as_deref());
        let (refs, next_cursor, total_count) = paginate_refs(filtered, input.cursor, input.limit);
        Ok(VcsListRefsResult {
            refs,
            is_repo: true,
            has_primary_remote: snapshot.has_primary_remote,
            next_cursor,
            total_count,
        })
    }

    // -----------------------------------------------------------------------------------------
    // Worktrees
    // -----------------------------------------------------------------------------------------

    async fn create_worktree_inner(&self, input: &VcsCreateWorktreeInput, options: &CreateWorktreeOptions) -> Result<VcsCreateWorktreeResult, GitCommandError> {
        let target_branch = input.new_ref_name.clone().unwrap_or_else(|| input.ref_name.clone());
        let sanitized = target_branch.replace('/', "-");
        let worktree_path = match &input.path {
            Some(path) => path.clone(),
            None => path_string(&zc_core::paths::normalize_lexically(
                &self.inner.worktrees_dir.join(basename(&input.cwd)).join(&sanitized),
            )),
        };
        let mut args = vec!["worktree".to_owned(), "add".to_owned()];
        if let Some(new_ref) = &input.new_ref_name {
            args.push("-b".into());
            args.push(new_ref.clone());
        }
        args.push(worktree_path.clone());
        args.push(input.ref_name.clone());
        let progress = &options.progress;
        let workers = self.read_config_value(&input.cwd, "checkout.workers").await?.unwrap_or_else(|| "0".into());
        let mut full = vec!["-c".to_owned(), format!("checkout.workers={workers}")];
        full.extend(args);
        let mut opts = GitOpts::default().fallback("git worktree add failed").timeout_ms(WORKTREE_ADD_TIMEOUT_MS);
        if let Some(on_progress) = progress.on_checkout_progress.clone() {
            // Git only prints checkout progress to a pipe once GIT_PROGRESS_DELAY elapsed.
            opts.env = Some(env([("GIT_PROGRESS_DELAY", "0"), ("LC_ALL", "C")]));
            opts.progress = Some(ExecuteGitProgress {
                on_stderr_line: Some(Arc::new(move |line: &str| {
                    if let Some(parsed) = parse_git_checkout_progress_line(line) {
                        on_progress(CheckoutProgress {
                            percent: parsed.percent,
                            completed: parsed.completed,
                            total: parsed.total,
                        });
                    }
                })),
                ..ExecuteGitProgress::default()
            });
        }
        self.run_git("GitVcsDriver.createWorktree", &input.cwd, full, opts).await?;
        if let Some(claimed) = &progress.on_worktree_claimed {
            claimed(&worktree_path);
        }

        // `git worktree add` leaves submodules empty; populate them best-effort.
        let has_submodules = path_exists(Path::new(&worktree_path).join(".gitmodules")).await;
        let (mode, source) = if !has_submodules {
            (WorktreeSubmodules::None, SettingSource::Environment)
        } else {
            let project_file = if options.submodules.is_some() {
                None
            } else {
                match tokio::fs::read_to_string(Path::new(&worktree_path).join(T3_PROJECT_FILE_NAME)).await {
                    Ok(contents) => {
                        let parsed = parse_t3_project_file(&contents);
                        if parsed.is_none() {
                            tracing::warn!(
                                worktree_path = %worktree_path,
                                "t3.json is invalid; initializing submodules recursively"
                            );
                        }
                        parsed
                    }
                    Err(_) => None,
                }
            };
            resolve_worktree_submodules(options.submodules, project_file)
        };
        if has_submodules && mode == WorktreeSubmodules::None {
            if let Some(disabled) = &progress.on_submodules_disabled {
                disabled(if source == SettingSource::T3Json {
                    SubmodulesDisabledSource::T3Json
                } else {
                    SubmodulesDisabledSource::Settings
                });
            }
        }
        if mode != WorktreeSubmodules::None {
            if let Some(started) = &progress.on_submodules_started {
                started();
            }
            let args = if mode == WorktreeSubmodules::Recursive {
                s(&["submodule", "update", "--init", "--recursive"])
            } else {
                s(&["submodule", "update", "--init"])
            };
            let opts = match progress.on_submodule_line.clone() {
                Some(on_line) => {
                    let line: LineCallback = Arc::new(move |l: &str| on_line(l));
                    GitOpts {
                        progress: Some(ExecuteGitProgress {
                            on_stdout_line: Some(line.clone()),
                            on_stderr_line: Some(line),
                            ..ExecuteGitProgress::default()
                        }),
                        ..GitOpts::default().with_env(env([("LC_ALL", "C")]))
                    }
                }
                None => GitOpts::default(),
            };
            match self.run_git("GitVcsDriver.createWorktree.updateSubmodules", &worktree_path, args, opts).await {
                Ok(()) => {
                    if let Some(finished) = &progress.on_submodules_finished {
                        finished(true, None);
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        worktree_path = %worktree_path,
                        detail = %error.detail,
                        "worktree submodule checkout failed; submodule paths are empty"
                    );
                    if let Some(finished) = &progress.on_submodules_finished {
                        finished(false, Some(&error.message()));
                    }
                }
            }
        }

        if let (Some(new_ref), Some(base_ref)) = (&input.new_ref_name, &input.base_ref_name) {
            let mut names = self.list_remote_names(&input.cwd).await.unwrap_or_default();
            sort_longest_first(&mut names);
            let base_branch = parse_remote_ref_with_remote_names(base_ref, &names)
                .map(|p| p.branch_name)
                .unwrap_or_else(|| base_ref.clone());
            self.run_git(
                "GitVcsDriver.createWorktree.configureBaseRef",
                &input.cwd,
                vec!["config".into(), format!("branch.{new_ref}.gh-merge-base"), base_branch],
                GitOpts::default(),
            )
            .await?;
        }
        Ok(VcsCreateWorktreeResult {
            worktree: VcsWorktree {
                path: worktree_path,
                ref_name: target_branch,
            },
        })
    }

    /// `createWorktree(input, options?)`.
    pub async fn create_worktree(&self, input: &VcsCreateWorktreeInput, options: &CreateWorktreeOptions) -> Result<VcsCreateWorktreeResult, GitCommandError> {
        self.with_invalidation(&input.cwd, self.create_worktree_inner(input, options)).await
    }

    async fn remove_worktree_inner(&self, input: &VcsRemoveWorktreeInput) -> Result<(), GitCommandError> {
        let mut args = s(&["worktree", "remove"]);
        if input.force == Some(true) {
            args.push("--force".into());
        }
        args.push(input.path.clone());
        let argument_count = args.len();
        let result = self
            .git_c(
                "GitVcsDriver.removeWorktree",
                &input.cwd,
                args,
                GitOpts::allow_non_zero().timeout_ms(WORKTREE_REMOVE_TIMEOUT_MS),
            )
            .await?;
        if result.exit_code == 0 {
            return Ok(());
        }
        // A worktree that is already gone is a no-op; prune its stale registration.
        if is_missing_worktree_stderr(&result.stderr) && !path_exists(&input.path).await {
            return self.prune_worktrees_inner(&input.cwd).await;
        }
        tracing::warn!(
            "GitVcsDriver.removeWorktree: git worktree remove exited with code {} for {} (stderr length {}).",
            result.exit_code,
            input.path,
            js_length(&result.stderr)
        );
        Err(GitCommandError {
            exit_code: Some(result.exit_code),
            stdout_length: Some(js_length(&result.stdout)),
            stderr_length: Some(js_length(&result.stderr)),
            ..GitCommandError::git("GitVcsDriver.removeWorktree", &input.cwd, argument_count, "git worktree remove failed")
        })
    }

    /// `removeWorktree(input)`.
    pub async fn remove_worktree(&self, input: &VcsRemoveWorktreeInput) -> Result<(), GitCommandError> {
        self.with_invalidation(&input.cwd, self.remove_worktree_inner(input)).await
    }

    async fn prune_worktrees_inner(&self, cwd: &str) -> Result<(), GitCommandError> {
        self.run_git(
            "GitVcsDriver.pruneWorktrees",
            cwd,
            s(&["worktree", "prune"]),
            GitOpts::default().timeout_ms(15_000).fallback("git worktree prune failed"),
        )
        .await
    }

    /// `pruneWorktrees({cwd})`.
    pub async fn prune_worktrees(&self, cwd: &str) -> Result<(), GitCommandError> {
        self.with_invalidation(cwd, self.prune_worktrees_inner(cwd)).await
    }

    // -----------------------------------------------------------------------------------------
    // Pull requests, fetches, remotes
    // -----------------------------------------------------------------------------------------

    /// `fetchPullRequestBranch({cwd, prNumber, branch})`.
    pub async fn fetch_pull_request_branch(&self, cwd: &str, pr_number: u64, branch: &str) -> Result<(), GitCommandError> {
        self.with_invalidation(cwd, async {
            let remote = self.resolve_primary_remote_name(cwd).await?;
            self.run_git(
                "GitVcsDriver.fetchPullRequestBranch",
                cwd,
                vec![
                    "fetch".into(),
                    "--quiet".into(),
                    "--no-tags".into(),
                    remote,
                    format!("+refs/pull/{pr_number}/head:refs/heads/{branch}"),
                ],
                GitOpts::default().fallback("git fetch pull request branch failed"),
            )
            .await
        })
        .await
    }

    /// `resolveCommit({cwd, revision})`.
    pub async fn resolve_commit(&self, cwd: &str, revision: &str) -> Result<String, GitCommandError> {
        Ok(self
            .stdout(
                "GitVcsDriver.resolveCommit",
                cwd,
                vec!["rev-parse".into(), "--verify".into(), format!("{revision}^{{commit}}")],
                false,
            )
            .await?
            .trim()
            .to_owned())
    }

    /// `fetchPullRequestHeadCommit({cwd, prNumber})`: the pull head into `FETCH_HEAD`.
    pub async fn fetch_pull_request_head_commit(&self, cwd: &str, pr_number: u64) -> Result<String, GitCommandError> {
        let remote = self.resolve_primary_remote_name(cwd).await?;
        self.run_git(
            "GitVcsDriver.fetchPullRequestHeadCommit",
            cwd,
            vec![
                "fetch".into(),
                "--quiet".into(),
                "--no-tags".into(),
                remote,
                format!("refs/pull/{pr_number}/head"),
            ],
            GitOpts::default().fallback("git fetch pull request head failed"),
        )
        .await?;
        self.resolve_commit(cwd, "FETCH_HEAD").await
    }

    /// `refreshCheckedOutBranch({cwd, targetCommit, resetWhenHeadCommit?})`.
    pub async fn refresh_checked_out_branch(
        &self,
        cwd: &str,
        target_commit: &str,
        reset_when_head_commit: Option<&str>,
    ) -> Result<GitRefreshCheckedOutBranchResult, GitCommandError> {
        self.with_invalidation(cwd, async {
            let head = self.resolve_commit(cwd, "HEAD").await?;
            if head == target_commit {
                return Ok(GitRefreshCheckedOutBranchResult {
                    head_commit: head,
                    moved: false,
                    on_target: true,
                });
            }
            let changes = self
                .stdout("GitVcsDriver.refreshCheckedOutBranch.status", cwd, s(&["status", "--porcelain"]), false)
                .await?;
            if !changes.trim().is_empty() {
                return Ok(GitRefreshCheckedOutBranchResult {
                    head_commit: head,
                    moved: false,
                    on_target: false,
                });
            }
            let is_ancestor = self
                .git(
                    "GitVcsDriver.refreshCheckedOutBranch.isAncestor",
                    cwd,
                    vec!["merge-base".into(), "--is-ancestor".into(), head.clone(), target_commit.into()],
                    GitOpts::allow_non_zero(),
                )
                .await?
                .exit_code
                == 0;
            if !is_ancestor && Some(head.as_str()) != reset_when_head_commit {
                return Ok(GitRefreshCheckedOutBranchResult {
                    head_commit: head,
                    moved: false,
                    on_target: false,
                });
            }
            if !is_ancestor {
                self.run_git(
                    "GitVcsDriver.refreshCheckedOutBranch.keepPrevious",
                    cwd,
                    vec!["update-ref".into(), "refs/t3code/pre-refresh".into(), head.clone()],
                    GitOpts::default().fallback("git failed to record the previous checkout commit"),
                )
                .await?;
            }
            self.run_git(
                "GitVcsDriver.refreshCheckedOutBranch.move",
                cwd,
                if is_ancestor {
                    vec!["merge".into(), "--ff-only".into(), target_commit.into()]
                } else {
                    vec!["reset".into(), "--merge".into(), target_commit.into()]
                },
                GitOpts::default()
                    .timeout_ms(30_000)
                    .fallback("git failed to move the checkout onto the pull request head"),
            )
            .await?;
            Ok(GitRefreshCheckedOutBranchResult {
                head_commit: target_commit.to_owned(),
                moved: true,
                on_target: true,
            })
        })
        .await
    }

    /// `ensureRemote({cwd, preferredName, url})`: an existing remote for the URL (any
    /// transport), else a new one under a free sanitized name.
    pub async fn ensure_remote(&self, cwd: &str, preferred_name: &str, url: &str) -> Result<String, GitCommandError> {
        self.with_invalidation(cwd, self.ensure_remote_inner(cwd, preferred_name, url)).await
    }

    async fn fetch_remote_inner(&self, cwd: &str, remote_name: &str, ref_name: Option<&str>) -> Result<(), GitCommandError> {
        let args = vec!["fetch".to_owned(), "--quiet".to_owned(), remote_name.to_owned()];
        let fallback = format!("git fetch {remote_name} failed");
        let fail = |args: &[String], result: &ExecuteGitResult| GitCommandError {
            exit_code: Some(result.exit_code),
            stdout_length: Some(js_length(&result.stdout)),
            stderr_length: Some(js_length(&result.stderr)),
            ..GitCommandError::git(
                "GitVcsDriver.fetchRemote",
                cwd,
                args.len(),
                fetch_failure_detail(&result.stderr).map(str::to_owned).unwrap_or_else(|| fallback.clone()),
            )
        };
        let opts = GitOpts::allow_non_zero().with_env(non_interactive_env()).fallback(&fallback);
        let fetch_all = || async {
            let result = self.git_c("GitVcsDriver.fetchRemote", cwd, args.clone(), opts.clone()).await?;
            if result.exit_code == 0 {
                Ok(())
            } else {
                Err(fail(&args, &result))
            }
        };
        let Some(ref_name) = ref_name else {
            return fetch_all().await;
        };
        let branch = parse_remote_ref_with_remote_names(ref_name, &[remote_name])
            .map(|p| p.branch_name)
            .unwrap_or_else(|| ref_name.to_owned());
        let mut scoped = args.clone();
        scoped.push(format!("+refs/heads/{branch}:refs/remotes/{remote_name}/{branch}"));
        let result = self.git_c("GitVcsDriver.fetchRemote", cwd, scoped.clone(), opts.clone()).await?;
        if result.exit_code == 0 {
            return Ok(());
        }
        let missing = format!("fatal: couldn't find remote ref refs/heads/{branch}");
        if result.stderr.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).any(|line| line == missing) {
            return fetch_all().await;
        }
        Err(fail(&scoped, &result))
    }

    /// `fetchRemote({cwd, remoteName, refName?})`: a scoped fetch of one branch when given
    /// (falling back to the whole remote when the branch does not exist there).
    pub async fn fetch_remote(&self, cwd: &str, remote_name: &str, ref_name: Option<&str>) -> Result<(), GitCommandError> {
        self.with_invalidation(cwd, self.fetch_remote_inner(cwd, remote_name, ref_name)).await
    }

    /// `resolveRemoteTrackingCommit({cwd, refName, fallbackRemoteName})`.
    pub async fn resolve_remote_tracking_commit(&self, cwd: &str, ref_name: &str, fallback_remote_name: &str) -> Result<RemoteTrackingCommit, GitCommandError> {
        let mut names = self.list_remote_names(cwd).await?;
        sort_longest_first(&mut names);
        let remote_ref_name = parse_remote_ref_with_remote_names(ref_name, &names)
            .map(|p| p.remote_ref)
            .unwrap_or_else(|| format!("{fallback_remote_name}/{ref_name}"));
        let commit_sha = self
            .stdout(
                "GitVcsDriver.resolveRemoteTrackingCommit",
                cwd,
                vec!["rev-parse".into(), "--verify".into(), format!("refs/remotes/{remote_ref_name}^{{commit}}")],
                false,
            )
            .await?
            .trim()
            .to_owned();
        Ok(RemoteTrackingCommit { commit_sha, remote_ref_name })
    }

    /// `fetchRemoteBranch({cwd, remoteName, remoteBranch, localBranch})`.
    pub async fn fetch_remote_branch(&self, cwd: &str, remote_name: &str, remote_branch: &str, local_branch: &str) -> Result<(), GitCommandError> {
        self.with_invalidation(cwd, async {
            self.run_git(
                "GitVcsDriver.fetchRemoteBranch.fetch",
                cwd,
                vec![
                    "fetch".into(),
                    "--quiet".into(),
                    "--no-tags".into(),
                    remote_name.into(),
                    format!("+refs/heads/{remote_branch}:refs/remotes/{remote_name}/{remote_branch}"),
                ],
                GitOpts::default(),
            )
            .await?;
            let exists = self.branch_exists(cwd, local_branch).await?;
            let target = format!("{remote_name}/{remote_branch}");
            self.run_git(
                "GitVcsDriver.fetchRemoteBranch.materialize",
                cwd,
                if exists {
                    vec!["branch".into(), "--force".into(), local_branch.into(), target]
                } else {
                    vec!["branch".into(), local_branch.into(), target]
                },
                GitOpts::default(),
            )
            .await
        })
        .await
    }

    /// `fetchRemoteTrackingBranch({cwd, remoteName, remoteBranch})`.
    pub async fn fetch_remote_tracking_branch(&self, cwd: &str, remote_name: &str, remote_branch: &str) -> Result<(), GitCommandError> {
        self.with_invalidation(cwd, async {
            self.run_git(
                "GitVcsDriver.fetchRemoteTrackingBranch",
                cwd,
                vec![
                    "fetch".into(),
                    "--quiet".into(),
                    "--no-tags".into(),
                    remote_name.into(),
                    format!("+refs/heads/{remote_branch}:refs/remotes/{remote_name}/{remote_branch}"),
                ],
                GitOpts::default(),
            )
            .await
        })
        .await
    }

    /// `setBranchUpstream({cwd, branch, remoteName, remoteBranch})`.
    pub async fn set_branch_upstream(&self, cwd: &str, branch: &str, remote_name: &str, remote_branch: &str) -> Result<(), GitCommandError> {
        self.with_invalidation(cwd, async {
            self.run_git(
                "GitVcsDriver.setBranchUpstream",
                cwd,
                vec![
                    "branch".into(),
                    "--set-upstream-to".into(),
                    format!("{remote_name}/{remote_branch}"),
                    branch.into(),
                ],
                GitOpts::default(),
            )
            .await
        })
        .await
    }

    /// `renameBranch({cwd, oldBranch, newBranch})`: returns the name used (`-N` suffixed when
    /// taken).
    pub async fn rename_branch(&self, cwd: &str, old_branch: &str, new_branch: &str) -> Result<String, GitCommandError> {
        self.with_invalidation(cwd, async {
            if old_branch == new_branch {
                return Ok(new_branch.to_owned());
            }
            let target = self.resolve_available_branch_name(cwd, new_branch).await?;
            self.run_git(
                "GitVcsDriver.renameBranch",
                cwd,
                vec!["branch".into(), "-m".into(), "--".into(), old_branch.into(), target.clone()],
                GitOpts::default().timeout_ms(10_000).fallback("git branch rename failed"),
            )
            .await?;
            Ok(target)
        })
        .await
    }

    async fn show_ref_exists(&self, operation: &str, cwd: &str, full_ref: String) -> Result<bool, GitCommandError> {
        Ok(self
            .git(
                operation,
                cwd,
                vec!["show-ref".into(), "--verify".into(), "--quiet".into(), full_ref],
                GitOpts::allow_non_zero().timeout_ms(5_000),
            )
            .await?
            .exit_code
            == 0)
    }

    async fn switch_ref_inner(&self, input: &VcsSwitchRefInput) -> Result<VcsSwitchRefResult, GitCommandError> {
        let cwd = input.cwd.as_str();
        let name = input.ref_name.as_str();
        let (local_exists, remote_exists) = tokio::try_join!(
            self.show_ref_exists("GitVcsDriver.switchRef.localInputExists", cwd, format!("refs/heads/{name}")),
            self.show_ref_exists("GitVcsDriver.switchRef.remoteExists", cwd, format!("refs/remotes/{name}"))
        )?;
        let local_tracking = if remote_exists {
            let result = self
                .git(
                    "GitVcsDriver.switchRef.localTrackingBranch",
                    cwd,
                    s(&["for-each-ref", "--format=%(refname:short)\t%(upstream:short)", "refs/heads"]),
                    GitOpts::allow_non_zero().timeout_ms(5_000),
                )
                .await?;
            if result.exit_code == 0 {
                parse_tracking_branch_by_upstream_ref(&result.stdout, name)
            } else {
                None
            }
        } else {
            None
        };
        let candidate = derive_local_branch_from_remote_ref(name);
        let candidate_exists = match (&candidate, remote_exists) {
            (Some(candidate), true) => {
                self.show_ref_exists("GitVcsDriver.switchRef.localTrackedBranchTargetExists", cwd, format!("refs/heads/{candidate}"))
                    .await?
            }
            _ => false,
        };
        let mut checkout_args = if local_exists || (remote_exists && local_tracking.is_none() && candidate_exists) {
            vec!["checkout".to_owned(), name.to_owned()]
        } else if remote_exists && local_tracking.is_none() {
            vec!["checkout".to_owned(), "--track".to_owned(), name.to_owned()]
        } else if let (true, Some(tracking)) = (remote_exists, &local_tracking) {
            vec!["checkout".to_owned(), tracking.clone()]
        } else {
            vec!["checkout".to_owned(), name.to_owned()]
        };
        // A stale ref must not turn into a path checkout that discards local edits.
        checkout_args.push("--".into());
        self.run_git(
            "GitVcsDriver.switchRef.checkout",
            cwd,
            checkout_args,
            GitOpts::default().timeout_ms(10_000).fallback("git checkout failed"),
        )
        .await?;
        let current = self
            .stdout("GitVcsDriver.switchRef.currentBranch", cwd, s(&["branch", "--show-current"]), false)
            .await?;
        let current = current.trim();
        Ok(VcsSwitchRefResult {
            ref_name: (!current.is_empty()).then(|| current.to_owned()),
        })
    }

    /// `switchRef({cwd, refName})`.
    pub async fn switch_ref(&self, input: &VcsSwitchRefInput) -> Result<VcsSwitchRefResult, GitCommandError> {
        self.with_invalidation(&input.cwd, self.switch_ref_inner(input)).await
    }

    /// `createRef({cwd, refName, switchRef?})`.
    pub async fn create_ref(&self, input: &VcsCreateRefInput) -> Result<VcsCreateRefResult, GitCommandError> {
        self.with_invalidation(&input.cwd, async {
            self.run_git(
                "GitVcsDriver.createRef",
                &input.cwd,
                vec!["branch".into(), input.ref_name.clone()],
                GitOpts::default().timeout_ms(10_000).fallback("git branch create failed"),
            )
            .await?;
            if input.switch_ref == Some(true) {
                self.switch_ref_inner(&VcsSwitchRefInput {
                    cwd: input.cwd.clone(),
                    ref_name: input.ref_name.clone(),
                })
                .await?;
            }
            Ok(VcsCreateRefResult {
                ref_name: input.ref_name.clone(),
            })
        })
        .await
    }

    /// `initRepo({cwd})`: `git init`, then forget what was cached about `cwd`.
    pub async fn init_repo(&self, cwd: &str) -> Result<(), GitCommandError> {
        let result = self
            .run_git(
                "GitVcsDriver.initRepo",
                cwd,
                s(&["init"]),
                GitOpts::default().timeout_ms(10_000).fallback("git init failed"),
            )
            .await;
        let key = normalize_cwd_key(cwd);
        self.inner.repository_paths_refresh.invalidate(&key);
        self.inner.repository_paths.invalidate(&key);
        let _ = self.invalidate_list_refs_snapshot(cwd).await;
        result
    }

    /// `listLocalBranchNames(cwd)`.
    pub async fn list_local_branch_names(&self, cwd: &str) -> Result<Vec<String>, GitCommandError> {
        let stdout = self
            .stdout(
                "GitVcsDriver.listLocalBranchNames",
                cwd,
                s(&["branch", "--list", "--no-column", "--format=%(refname:short)"]),
                false,
            )
            .await?;
        Ok(stdout.split('\n').map(str::trim).filter(|b| !b.is_empty()).map(str::to_owned).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_refresh_cooldown_doubles_up_to_fifteen_minutes() {
        assert_eq!(status_upstream_refresh_failure_cooldown(1), Duration::from_secs(30));
        assert_eq!(status_upstream_refresh_failure_cooldown(2), Duration::from_secs(60));
        assert_eq!(status_upstream_refresh_failure_cooldown(5), Duration::from_secs(480));
        assert_eq!(status_upstream_refresh_failure_cooldown(6), Duration::from_secs(900));
        assert_eq!(status_upstream_refresh_failure_cooldown(60), Duration::from_secs(900));
    }

    #[test]
    fn fetch_cwd_strips_dot_git() {
        assert_eq!(fetch_cwd_for("/repo/.git"), "/repo");
        assert_eq!(fetch_cwd_for("/repo.git"), "/repo.git");
    }

    #[test]
    fn root_containment() {
        assert!(is_path_within_root(Path::new("/a/b"), Path::new("/a/b")));
        assert!(is_path_within_root(Path::new("/a/b"), Path::new("/a/b/c/d")));
        assert!(!is_path_within_root(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(!is_path_within_root(Path::new("/a/b"), Path::new("/a/b/../c")));
    }
}
