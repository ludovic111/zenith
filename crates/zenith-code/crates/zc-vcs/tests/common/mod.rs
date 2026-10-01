//! Shared fixtures for the zc-vcs integration tests: temporary repositories with local git
//! config only, isolated from the user's global and system config.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};

use async_trait::async_trait;
use zc_vcs::errors::GitCommandError;
use zc_vcs::git_exec::{ExecuteGitInput, ExecuteGitResult, GitExecutor, GitInterceptor};
use zc_vcs::GitVcsDriver;

static ISOLATE: Once = Once::new();

/// Point git at an empty global config (plus `protocol.file.allow` for local submodule
/// fixtures) and ignore the system config. Runs once, before any test spawns git.
pub fn isolate_git() {
    ISOLATE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("zc-vcs-test-home-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("gitconfig");
        std::fs::write(
            &config,
            "[protocol \"file\"]\n\tallow = always\n[advice]\n\tdefaultBranchName = false\n[commit]\n\tgpgsign = false\n[tag]\n\tgpgsign = false\n",
        )
        .unwrap();
        // SAFETY: runs once, before any test of this binary spawns a process or reads the
        // environment (every test calls `isolate_git` first, and `call_once` blocks the others).
        unsafe {
            std::env::set_var("GIT_CONFIG_GLOBAL", &config);
            std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
            std::env::remove_var("GIT_DIR");
            std::env::remove_var("GIT_WORK_TREE");
            std::env::remove_var("GIT_INDEX_FILE");
        }
    });
}

/// A temporary directory (canonical path: macOS temp dirs sit behind a `/private` symlink).
pub struct Tmp {
    _dir: tempfile::TempDir,
    pub path: PathBuf,
}

impl Tmp {
    pub fn new(prefix: &str) -> Self {
        isolate_git();
        let dir = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        Self { _dir: dir, path }
    }

    pub fn str(&self) -> &str {
        self.path.to_str().unwrap()
    }

    pub fn join(&self, rel: &str) -> String {
        self.path.join(rel).to_string_lossy().into_owned()
    }
}

/// Run git synchronously; panics on failure. Returns trimmed stdout.
pub fn git(cwd: impl AsRef<Path>, args: &[&str]) -> String {
    isolate_git();
    let output = Command::new("git").args(args).current_dir(cwd.as_ref()).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed in {}: {}",
        cwd.as_ref().display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Run git, returning the exit code (no panic).
pub fn git_status(cwd: impl AsRef<Path>, args: &[&str]) -> i32 {
    Command::new("git")
        .args(args)
        .current_dir(cwd.as_ref())
        .output()
        .unwrap()
        .status
        .code()
        .unwrap_or(-1)
}

pub fn write(cwd: impl AsRef<Path>, rel: &str, contents: &str) {
    let path = cwd.as_ref().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

pub fn read(cwd: impl AsRef<Path>, rel: &str) -> String {
    std::fs::read_to_string(cwd.as_ref().join(rel)).unwrap()
}

/// `initRepoWithCommit`: `git init`, local identity, a README commit. Returns the branch.
pub fn init_repo_with_commit(cwd: impl AsRef<Path>) -> String {
    let cwd = cwd.as_ref();
    git(cwd, &["init"]);
    git(cwd, &["config", "user.email", "test@test.com"]);
    git(cwd, &["config", "user.name", "Test"]);
    write(cwd, "README.md", "# test\n");
    git(cwd, &["add", "."]);
    git(cwd, &["commit", "-m", "initial commit"]);
    git(cwd, &["branch", "--show-current"])
}

/// Commit everything in `cwd` with a message.
pub fn commit_all(cwd: impl AsRef<Path>, message: &str) {
    git(cwd.as_ref(), &["add", "."]);
    git(cwd.as_ref(), &["commit", "-m", message]);
}

/// A driver whose worktrees directory is a fresh temp dir.
pub fn driver() -> (GitVcsDriver, Tmp) {
    let worktrees = Tmp::new("zc-vcs-worktrees-");
    (GitVcsDriver::new(&worktrees.path), worktrees)
}

pub fn driver_with(interceptor: Arc<dyn GitInterceptor>) -> (GitVcsDriver, Tmp) {
    let worktrees = Tmp::new("zc-vcs-worktrees-");
    (
        GitVcsDriver::with_executor(&worktrees.path, GitExecutor::new().with_interceptor(interceptor)),
        worktrees,
    )
}

/// A recording interceptor that lets every command run, unless `respond` answers it.
#[derive(Default)]
pub struct Recorder {
    pub calls: Mutex<Vec<ExecuteGitInput>>,
    #[allow(clippy::type_complexity)]
    pub respond: Option<Box<dyn Fn(&ExecuteGitInput) -> Option<Result<ExecuteGitResult, GitCommandError>> + Send + Sync>>,
}

impl Recorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn responding(respond: impl Fn(&ExecuteGitInput) -> Option<Result<ExecuteGitResult, GitCommandError>> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            respond: Some(Box::new(respond)),
        })
    }

    pub fn args(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().iter().map(|c| c.args.clone()).collect()
    }

    pub fn count(&self, predicate: impl Fn(&[String]) -> bool) -> usize {
        self.args().iter().filter(|a| predicate(a)).count()
    }

    pub fn clear(&self) {
        self.calls.lock().unwrap().clear();
    }
}

#[async_trait]
impl GitInterceptor for Recorder {
    async fn intercept(&self, input: &ExecuteGitInput) -> Option<Result<ExecuteGitResult, GitCommandError>> {
        self.calls.lock().unwrap().push(input.clone());
        self.respond.as_ref().and_then(|respond| respond(input))
    }
}

/// `makeNonRepositoryHandle`: exit 128, `fatal: not a git repository` on stderr.
pub fn not_a_repository() -> Option<Result<ExecuteGitResult, GitCommandError>> {
    Some(Ok(ExecuteGitResult {
        exit_code: 128,
        stdout: String::new(),
        stderr: "fatal: not a git repository".into(),
        stdout_truncated: false,
        stderr_truncated: false,
    }))
}

/// `makeSuccessfulHandle(stdout)`.
pub fn succeed_with(stdout: &str) -> Option<Result<ExecuteGitResult, GitCommandError>> {
    Some(Ok(ExecuteGitResult {
        exit_code: 0,
        stdout: stdout.into(),
        ..ExecuteGitResult::default()
    }))
}

pub fn has(args: &[String], value: &str) -> bool {
    args.iter().any(|a| a == value)
}
