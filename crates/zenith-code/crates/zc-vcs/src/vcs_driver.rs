//! The generic VCS driver (`vcs/VcsDriver.ts`) and its git implementation
//! (`vcs/GitVcsDriver.ts` `makeVcsDriverShape`): repository detection, workspace file
//! listing, remotes, ignore filtering, `init`, and the checkpoint primitives.
//!
//! Unlike [`crate::driver_core`], this driver goes through zc-core's [`VcsProcess`] (the shared
//! 8-process / 4-`gh` semaphores, typed `VcsError`s, transient-failure retries for checkpoint
//! capture), running `git -C <cwd> …` from the server's own working directory, like TS.
//!
//! Checkpoints (plan §5.5): a private index `<commonDir>/t3-checkpoint-index-<uuid>` seeded from
//! the user index when its stat data can be trusted (or rebuilt from `HEAD`), `add -A` (sparse
//! aware, skipping commit-less nested repositories on failure), `write-tree`, `commit-tree`
//! as "T3 Code", then `update-ref`, every write with `core.fsync=objects,reference`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use zc_core::process::OutputMode;
use zc_core::vcs_process::{VcsProcess, VcsProcessInput, VcsProcessOutput, CHECKPOINT_CAPTURE_OPERATION};

use crate::contracts::*;
use crate::errors::VcsError;
use crate::parse::{chunk_paths_for_git_check_ignore, parse_git_remote_verbose_output, split_null_separated_paths};

const WORKSPACE_FILES_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const CHECKPOINT_RECOVERY_MAX_CANDIDATES: usize = 64;
const CHECKPOINT_RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const CHECKPOINT_DIFF_MAX_OUTPUT_BYTES: usize = 10_000_000;
const WORKSPACE_GIT_HARDENED_CONFIG_ARGS: [&str; 4] = ["-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false"];
/// Checkpoint writes flush objects and refs before they are published (an unclean restart
/// could otherwise leave 0-byte refs under `refs/t3/**`).
const DURABLE_WRITE: [&str; 4] = ["-c", "core.fsync=objects,reference", "-c", "core.fsyncMethod=fsync"];
const INDEX_CONFIG: [&str; 4] = ["-c", "core.fsmonitor=false", "-c", "sparse.expectFilesOutsideOfPatterns=false"];

/// `VcsDiffCheckpointsInput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffCheckpointsInput {
    pub cwd: String,
    pub from_checkpoint_ref: String,
    pub to_checkpoint_ref: String,
    pub fallback_from_to_head: bool,
    pub ignore_whitespace: bool,
    /// `"numstat"` (`--numstat -z`, failing past the cap) instead of a patch (truncated).
    pub numstat: bool,
}

/// `VcsCheckpointOps`.
#[async_trait]
pub trait VcsCheckpointOps: Send + Sync {
    /// `captureCheckpoint({cwd, checkpointRef})`.
    async fn capture_checkpoint(&self, cwd: &str, checkpoint_ref: &str) -> Result<(), VcsError>;
    /// `hasCheckpointRef({cwd, checkpointRef})`.
    async fn has_checkpoint_ref(&self, cwd: &str, checkpoint_ref: &str) -> Result<bool, VcsError>;
    /// `restoreCheckpoint({cwd, checkpointRef, fallbackToHead?})`: false when there is nothing
    /// to restore.
    async fn restore_checkpoint(&self, cwd: &str, checkpoint_ref: &str, fallback_to_head: bool) -> Result<bool, VcsError>;
    /// `diffCheckpoints(input)`.
    async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, VcsError>;
    /// `deleteCheckpointRefs({cwd, checkpointRefs})`.
    async fn delete_checkpoint_refs(&self, cwd: &str, checkpoint_refs: &[String]) -> Result<(), VcsError>;
}

/// `VcsDriver` (`vcs/VcsDriver.ts`).
#[async_trait]
pub trait VcsDriver: Send + Sync {
    fn capabilities(&self) -> VcsDriverCapabilities;

    /// `execute(input)`: run the driver's CLI (the input's `command` is ignored).
    async fn execute(&self, input: VcsProcessInput) -> Result<VcsProcessOutput, VcsError>;

    fn checkpoints(&self) -> Option<&dyn VcsCheckpointOps> {
        None
    }

    async fn detect_repository(&self, cwd: &str) -> Result<Option<VcsRepositoryIdentity>, VcsError>;

    async fn is_inside_work_tree(&self, cwd: &str) -> Result<bool, VcsError>;

    async fn list_workspace_files(&self, cwd: &str) -> Result<VcsListWorkspaceFilesResult, VcsError>;

    async fn list_remotes(&self, cwd: &str) -> Result<VcsListRemotesResult, VcsError>;

    async fn filter_ignored_paths(&self, cwd: &str, relative_paths: &[String]) -> Result<Vec<String>, VcsError>;

    async fn init_repository(&self, input: &VcsInitInput) -> Result<(), VcsError>;

    /// `getDiffPreview` is optional: drivers that implement it return `true` here.
    fn supports_diff_preview(&self) -> bool {
        false
    }

    async fn get_diff_preview(&self, _input: &ReviewDiffPreviewInput) -> Result<ReviewDiffPreviewResult, VcsError> {
        Err(VcsError::UnsupportedOperation(crate::errors::VcsUnsupportedOperationError::new(
            "VcsDriver.getDiffPreview",
            self.capabilities().kind,
            "This VCS driver does not support review diff previews.",
        )))
    }
}

/// Options of `gitCommand`.
#[derive(Default)]
struct GitCommandOptions {
    stdin: Option<String>,
    env: Option<BTreeMap<String, Option<String>>>,
    allow_non_zero_exit: bool,
    timeout_ms: Option<u64>,
    max_output_bytes: Option<usize>,
    output_mode: Option<OutputMode>,
    append_truncation_marker: bool,
}

/// The git [`VcsDriver`].
#[derive(Clone)]
pub struct GitVcsProcessDriver {
    process: VcsProcess,
    spawn_cwd: PathBuf,
}

impl Default for GitVcsProcessDriver {
    fn default() -> Self {
        Self::new(VcsProcess::default())
    }
}

fn s(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

fn join(parts: &[&[&str]]) -> Vec<String> {
    parts.iter().flat_map(|p| p.iter().map(|v| (*v).to_owned())).collect()
}

/// Removes the private index (and its lock) however capture ends.
struct TempIndexGuard(PathBuf);

impl TempIndexGuard {
    fn cleanup(&self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(PathBuf::from(lock));
    }
}

impl Drop for TempIndexGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

impl GitVcsProcessDriver {
    pub fn new(process: VcsProcess) -> Self {
        Self {
            process,
            spawn_cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        }
    }

    /// `gitCommand`: `git -C <cwd> …` spawned from the server's working directory.
    async fn git(&self, operation: &str, cwd: &str, args: Vec<String>, options: GitCommandOptions) -> Result<VcsProcessOutput, VcsError> {
        let mut full = vec!["-C".to_owned(), cwd.to_owned()];
        full.extend(args);
        let input = VcsProcessInput {
            operation: operation.to_owned(),
            command: "git".into(),
            args: full,
            cwd: PathBuf::from(cwd),
            spawn_cwd: Some(self.spawn_cwd.clone()),
            stdin: options.stdin,
            on_stdout_chunk: None,
            env: options.env,
            allow_non_zero_exit: options.allow_non_zero_exit,
            timeout_ms: options.timeout_ms,
            max_output_bytes: options.max_output_bytes,
            output_mode: options.output_mode,
            append_truncation_marker: options.append_truncation_marker,
        };
        self.process.run(input).await.map_err(VcsError::from)
    }

    async fn resolve_head_commit(&self, cwd: &str) -> Result<Option<String>, VcsError> {
        let result = self
            .git(
                "GitVcsDriver.checkpoints.resolveHeadCommit",
                cwd,
                s(&["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    ..Default::default()
                },
            )
            .await?;
        let commit = result.stdout.trim();
        Ok((result.exit_code == 0 && !commit.is_empty()).then(|| commit.to_owned()))
    }

    async fn has_head_commit(&self, cwd: &str, env: Option<BTreeMap<String, Option<String>>>) -> Result<bool, VcsError> {
        Ok(self
            .git(
                "GitVcsDriver.checkpoints.hasHeadCommit",
                cwd,
                s(&["rev-parse", "--verify", "HEAD"]),
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    env,
                    ..Default::default()
                },
            )
            .await?
            .exit_code
            == 0)
    }

    async fn resolve_checkpoint_commit(&self, cwd: &str, checkpoint_ref: &str) -> Result<Option<String>, VcsError> {
        let result = self
            .git(
                "GitVcsDriver.checkpoints.resolveCheckpointCommit",
                cwd,
                vec!["rev-parse".into(), "--verify".into(), "--quiet".into(), format!("{checkpoint_ref}^{{commit}}")],
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    ..Default::default()
                },
            )
            .await?;
        let commit = result.stdout.trim();
        Ok((result.exit_code == 0 && !commit.is_empty()).then(|| commit.to_owned()))
    }

    async fn resolve_git_common_dir(&self, cwd: &str) -> Result<String, VcsError> {
        let result = self
            .git(
                "GitVcsDriver.checkpoints.resolveGitCommonDir",
                cwd,
                s(&["rev-parse", "--git-common-dir"]),
                GitCommandOptions::default(),
            )
            .await?;
        Ok(crate::git_exec::resolve_against(cwd, result.stdout.trim()).to_string_lossy().into_owned())
    }

    /// The `reusedIndex` probe: copy the user index when its stat data can be kept.
    async fn try_reuse_index(
        &self,
        cwd: &str,
        temp_index: &Path,
        commit_env: &BTreeMap<String, Option<String>>,
        sparse_checkout: bool,
    ) -> Result<bool, VcsError> {
        let operation = CHECKPOINT_CAPTURE_OPERATION;
        let index_path = self
            .git(
                operation,
                cwd,
                s(&["rev-parse", "--path-format=absolute", "--git-path", "index"]),
                GitCommandOptions::default(),
            )
            .await?;
        let index_path = index_path.stdout.trim().to_owned();
        let Ok(meta) = std::fs::metadata(&index_path) else {
            return Ok(false);
        };
        let Some(mtime_ms) = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
        else {
            return Ok(false);
        };
        // Stay below the source timestamp, preserving git's racy check.
        let index_time = (mtime_ms - 1).div_euclid(1000);
        if index_time <= 0 {
            return Ok(false);
        }
        if std::fs::copy(&index_path, temp_index).is_err() {
            return Ok(false);
        }
        // Retain stat data only where the copied index already matches HEAD.
        self.git(
            operation,
            cwd,
            join(&[&INDEX_CONFIG, &["read-tree", "--reset", "HEAD"]]),
            GitCommandOptions {
                env: Some(commit_env.clone()),
                ..Default::default()
            },
        )
        .await?;
        let time = std::time::UNIX_EPOCH + Duration::from_secs(index_time as u64);
        let utimes = std::fs::OpenOptions::new()
            .write(true)
            .open(temp_index)
            .and_then(|f| f.set_times(std::fs::FileTimes::new().set_accessed(time).set_modified(time)));
        if utimes.is_err() {
            return Ok(false);
        }

        #[derive(Default)]
        struct Scan {
            special_flags: bool,
            record_start: bool,
            skipped: bool,
            skipped_record: Vec<u8>,
            skipped_paths: Vec<String>,
        }
        let scan = Arc::new(Mutex::new(Scan {
            record_start: true,
            ..Scan::default()
        }));
        let sink = scan.clone();
        let on_chunk: zc_core::process::ChunkCallback = Arc::new(move |chunk: &[u8]| {
            let mut scan = sink.lock().unwrap_or_else(|p| p.into_inner());
            for &byte in chunk {
                if scan.record_start {
                    scan.skipped = byte == b'S';
                }
                if scan.skipped && sparse_checkout {
                    if byte != 0 {
                        scan.skipped_record.push(byte);
                    } else {
                        if scan.skipped_record.last() != Some(&b'/') {
                            let record = std::mem::take(&mut scan.skipped_record);
                            let name = record.get(2..).unwrap_or_default();
                            match std::str::from_utf8(name) {
                                Ok(name) => scan.skipped_paths.push(name.to_owned()),
                                Err(_) => scan.special_flags = true,
                            }
                        }
                        scan.skipped_record.clear();
                    }
                }
                if scan.record_start && (byte.is_ascii_lowercase() || (!sparse_checkout && byte == b'S')) {
                    scan.special_flags = true;
                }
                scan.record_start = byte == 0;
            }
        });
        // Inspect every tag; retain only skipped file paths for checking sparse rules.
        self.process
            .run(VcsProcessInput {
                operation: operation.into(),
                command: "git".into(),
                cwd: PathBuf::from(cwd),
                args: join(&[&INDEX_CONFIG, &["ls-files", "--full-name", "--sparse", "-v", "-z"]]),
                env: Some(commit_env.clone()),
                max_output_bytes: Some(4_096),
                output_mode: Some(OutputMode::Truncate),
                on_stdout_chunk: Some(on_chunk),
                ..VcsProcessInput::default()
            })
            .await?;
        let (skipped_paths, mut special_flags) = {
            let scan = scan.lock().unwrap_or_else(|p| p.into_inner());
            (scan.skipped_paths.clone(), scan.special_flags)
        };
        if !skipped_paths.is_empty() && !special_flags {
            let selected = self
                .git(
                    operation,
                    cwd,
                    join(&[&INDEX_CONFIG, &["sparse-checkout", "check-rules", "-z"]]),
                    GitCommandOptions {
                        stdin: Some(format!("{}\0", skipped_paths.join("\0"))),
                        env: Some(commit_env.clone()),
                        max_output_bytes: Some(1),
                        output_mode: Some(OutputMode::Truncate),
                        ..Default::default()
                    },
                )
                .await?;
            // Any selected skipped file has a manual flag, not a sparse exclusion.
            special_flags = !selected.stdout.is_empty() || selected.stdout_truncated;
        }
        // Sparse git clears skip-worktree for present files. Manual flags still need a reset.
        Ok(!special_flags)
    }

    async fn stage_files(
        &self,
        cwd: &str,
        commit_env: &BTreeMap<String, Option<String>>,
        sparse_checkout: bool,
        exclusions: &[String],
    ) -> Result<VcsProcessOutput, VcsError> {
        let mut args = join(&[&INDEX_CONFIG, &DURABLE_WRITE, &["add"]]);
        if sparse_checkout {
            args.push("--sparse".into());
        }
        args.extend(s(&["-A", "--", "."]));
        args.extend(exclusions.iter().cloned());
        self.git(
            CHECKPOINT_CAPTURE_OPERATION,
            cwd,
            args,
            GitCommandOptions {
                env: Some(commit_env.clone()),
                ..Default::default()
            },
        )
        .await
    }

    /// Recovery for an `add -A` that failed on commit-less nested repositories: exclude them
    /// and stage again. Any doubt returns the original error.
    async fn stage_without_empty_nested_repositories(
        &self,
        cwd: &str,
        commit_env: &BTreeMap<String, Option<String>>,
        sparse_checkout: bool,
        error: VcsError,
    ) -> Result<VcsProcessOutput, VcsError> {
        let untracked = self
            .git(
                CHECKPOINT_CAPTURE_OPERATION,
                cwd,
                s(&["ls-files", "--others", "--exclude-standard", "-z", "--", "."]),
                GitCommandOptions {
                    env: Some(commit_env.clone()),
                    max_output_bytes: Some(WORKSPACE_FILES_MAX_OUTPUT_BYTES),
                    ..Default::default()
                },
            )
            .await?;
        if untracked.stdout_truncated {
            return Err(error);
        }
        let candidates: Vec<String> = split_null_separated_paths(&untracked.stdout, untracked.stdout_truncated)
            .into_iter()
            .filter(|entry| entry.ends_with('/'))
            .collect();
        // Refuse excessive recovery work before probing any nested repositories.
        if candidates.len() > CHECKPOINT_RECOVERY_MAX_CANDIDATES {
            return Err(error);
        }
        // Discover each child's repository instead of inheriting the server's git bindings.
        let nested_env: BTreeMap<String, Option<String>> = [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ]
        .into_iter()
        .map(|key| (key.to_owned(), None))
        .collect();
        let mut exclusions = Vec::new();
        for entry in candidates {
            let nested = Path::new(cwd).join(&entry);
            let has_git = match tokio::fs::try_exists(nested.join(".git")).await {
                Ok(exists) => exists,
                Err(_) => return Err(error),
            };
            if has_git && !self.has_head_commit(&nested.to_string_lossy(), Some(nested_env.clone())).await? {
                exclusions.push(format!(":(exclude,literal){entry}"));
            }
        }
        if exclusions.is_empty() {
            return Err(error);
        }
        self.stage_files(cwd, commit_env, sparse_checkout, &exclusions).await
    }

    async fn capture(&self, cwd: &str, checkpoint_ref: &str) -> Result<(), VcsError> {
        let operation = CHECKPOINT_CAPTURE_OPERATION;
        let git_common_dir = self.resolve_git_common_dir(cwd).await?;
        let temp_index = Path::new(&git_common_dir).join(format!("t3-checkpoint-index-{}", uuid::Uuid::new_v4()));
        let guard = TempIndexGuard(temp_index.clone());
        let mut commit_env: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (key, value) in [
            ("GIT_INDEX_FILE", temp_index.to_string_lossy().into_owned()),
            ("GIT_AUTHOR_NAME", "T3 Code".to_owned()),
            ("GIT_AUTHOR_EMAIL", "t3code@users.noreply.github.com".to_owned()),
            ("GIT_COMMITTER_NAME", "T3 Code".to_owned()),
            ("GIT_COMMITTER_EMAIL", "t3code@users.noreply.github.com".to_owned()),
        ] {
            commit_env.insert(key.to_owned(), Some(value));
        }
        let allow = || GitCommandOptions {
            allow_non_zero_exit: true,
            ..Default::default()
        };

        let head_exists = self.has_head_commit(cwd, None).await?;
        let sparse_config = self.git(operation, cwd, s(&["config", "--bool", "core.sparseCheckout"]), allow()).await?;
        let mut sparse_checkout = sparse_config.stdout.trim() == "true";
        if sparse_checkout {
            static SPARSE_FLAG: OnceLock<Regex> = OnceLock::new();
            let help = self.git(operation, cwd, s(&["add", "-h"]), allow()).await?;
            sparse_checkout = SPARSE_FLAG
                .get_or_init(|| Regex::new(r"--(?:\[no-\])?sparse\b").unwrap())
                .is_match(&format!("{}{}", help.stdout, help.stderr));
        }
        if head_exists {
            let reused = self.try_reuse_index(cwd, &temp_index, &commit_env, sparse_checkout).await.unwrap_or(false);
            if !reused {
                if sparse_checkout {
                    let cone = self.git(operation, cwd, s(&["config", "--bool", "core.sparseCheckoutCone"]), allow()).await?;
                    // Rebuilding a non-cone index loses exclusions; do not publish false
                    // deletions.
                    if cone.stdout.trim() != "true" {
                        return Err(VcsError::exit(
                            operation,
                            "git read-tree",
                            cwd,
                            1,
                            "Cannot rebuild a checkpoint index for non-cone sparse checkout.",
                        ));
                    }
                }
                guard.cleanup();
                // A fresh sparse index represents excluded directories without marking them
                // deleted.
                let args = if sparse_checkout {
                    join(&[&INDEX_CONFIG, &["-c", "index.sparse=true", "read-tree", "--reset", "HEAD"]])
                } else {
                    s(&["read-tree", "HEAD"])
                };
                self.git(
                    operation,
                    cwd,
                    args,
                    GitCommandOptions {
                        env: Some(commit_env.clone()),
                        ..Default::default()
                    },
                )
                .await?;
            }
        }

        match self.stage_files(cwd, &commit_env, sparse_checkout, &[]).await {
            Ok(_) => {}
            Err(error @ VcsError::Process(zc_core::VcsProcessError::Exit { .. })) => {
                // One budget covers discovery, queued admission, probes and the retry.
                match tokio::time::timeout(
                    CHECKPOINT_RECOVERY_TIMEOUT,
                    self.stage_without_empty_nested_repositories(cwd, &commit_env, sparse_checkout, error.clone()),
                )
                .await
                {
                    Ok(result) => {
                        result?;
                    }
                    Err(_) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }

        let tree = self
            .git(
                operation,
                cwd,
                join(&[&INDEX_CONFIG, &DURABLE_WRITE, &["write-tree"]]),
                GitCommandOptions {
                    env: Some(commit_env.clone()),
                    ..Default::default()
                },
            )
            .await?;
        let tree_oid = tree.stdout.trim().to_owned();
        if tree_oid.is_empty() {
            return Err(VcsError::exit(
                operation,
                "git write-tree",
                cwd,
                0,
                "git write-tree returned an empty tree oid.",
            ));
        }
        let message = format!("t3 checkpoint ref={checkpoint_ref}");
        let commit = self
            .git(
                operation,
                cwd,
                join(&[&DURABLE_WRITE, &["commit-tree", &tree_oid, "-m", &message]]),
                GitCommandOptions {
                    env: Some(commit_env.clone()),
                    ..Default::default()
                },
            )
            .await?;
        let commit_oid = commit.stdout.trim().to_owned();
        if commit_oid.is_empty() {
            return Err(VcsError::exit(
                operation,
                "git commit-tree",
                cwd,
                0,
                "git commit-tree returned an empty commit oid.",
            ));
        }
        self.git(
            operation,
            cwd,
            join(&[&DURABLE_WRITE, &["update-ref", checkpoint_ref, &commit_oid]]),
            GitCommandOptions::default(),
        )
        .await?;
        drop(guard);
        Ok(())
    }

    async fn restore(&self, cwd: &str, checkpoint_ref: &str, fallback_to_head: bool) -> Result<bool, VcsError> {
        let operation = "GitVcsDriver.checkpoints.restoreCheckpoint";
        let mut commit = self.resolve_checkpoint_commit(cwd, checkpoint_ref).await?;
        if commit.is_none() && fallback_to_head {
            commit = self.resolve_head_commit(cwd).await?;
        }
        let Some(commit) = commit else {
            return Ok(false);
        };
        let tracked = self
            .git(
                operation,
                cwd,
                vec![
                    "ls-files".into(),
                    "--cached".into(),
                    format!("--with-tree={commit}"),
                    "-z".into(),
                    "--".into(),
                    ".".into(),
                ],
                GitCommandOptions::default(),
            )
            .await?;
        // An empty index and checkpoint have nothing for git restore's pathspec to match.
        if !tracked.stdout.is_empty() {
            self.git(
                operation,
                cwd,
                vec![
                    "restore".into(),
                    "--source".into(),
                    commit.clone(),
                    "--worktree".into(),
                    "--staged".into(),
                    "--".into(),
                    ".".into(),
                ],
                GitCommandOptions::default(),
            )
            .await?;
        }
        // Restoring away the last tracked file can remove a nested workspace directory.
        if let Err(io) = tokio::fs::create_dir_all(cwd).await {
            return Err(VcsError::exit(
                operation,
                "git restore",
                cwd,
                0,
                format!("Could not recreate the checkpoint workspace: {io}"),
            ));
        }
        let cleaned = self
            .git(
                operation,
                cwd,
                s(&["clean", "-fd", "--", "."]),
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    ..Default::default()
                },
            )
            .await?;
        if cleaned.exit_code != 0 {
            static FAILED_ROOT: OnceLock<Regex> = OnceLock::new();
            let failed_root = FAILED_ROOT.get_or_init(|| Regex::new(r"^warning: failed to remove \./: [^\n]+$").unwrap());
            // Git can remove every child, then fail trying to remove `./` itself.
            let emptied = cleaned.exit_code == 1
                && failed_root.is_match(cleaned.stderr.trim())
                && std::fs::read_dir(cwd).map(|mut entries| entries.next().is_none()).unwrap_or(false);
            if !emptied {
                let stderr = cleaned.stderr.trim();
                return Err(VcsError::exit(
                    operation,
                    "git clean",
                    cwd,
                    cleaned.exit_code,
                    if stderr.is_empty() {
                        "Could not clean the checkpoint workspace."
                    } else {
                        stderr
                    },
                ));
            }
        }
        if self.has_head_commit(cwd, None).await? {
            self.git(operation, cwd, s(&["reset", "--quiet", "--", "."]), GitCommandOptions::default())
                .await?;
        }
        Ok(true)
    }

    async fn diff(&self, input: &DiffCheckpointsInput) -> Result<String, VcsError> {
        let operation = "GitVcsDriver.checkpoints.diffCheckpoints";
        let mut from_revision = input.from_checkpoint_ref.clone();
        if input.fallback_from_to_head {
            match self.resolve_checkpoint_commit(&input.cwd, &input.from_checkpoint_ref).await? {
                Some(commit) => from_revision = commit,
                None => match self.resolve_head_commit(&input.cwd).await? {
                    Some(head) => from_revision = head,
                    None => {
                        return Err(VcsError::exit(
                            operation,
                            "git diff",
                            &input.cwd,
                            1,
                            "Checkpoint ref is unavailable for diff operation.",
                        ))
                    }
                },
            }
        }
        let mut args = vec!["diff".to_owned()];
        if input.numstat {
            args.extend(s(&["--numstat", "-z"]));
        } else {
            args.push("--patch".into());
        }
        args.extend(s(&["--no-color", "--no-ext-diff", "--no-textconv"]));
        args.extend(crate::driver_core::PATCH_RENDER_PREFIX_ARGS.iter().map(|a| (*a).to_owned()));
        if input.ignore_whitespace {
            args.push("--ignore-all-space".into());
        }
        args.push(format!("{from_revision}^{{commit}}"));
        args.push(format!("{}^{{commit}}", input.to_checkpoint_ref));
        let result = self
            .git(
                operation,
                &input.cwd,
                args,
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    max_output_bytes: Some(CHECKPOINT_DIFF_MAX_OUTPUT_BYTES),
                    output_mode: Some(if input.numstat { OutputMode::Error } else { OutputMode::Truncate }),
                    ..Default::default()
                },
            )
            .await?;
        if result.exit_code != 0 {
            let stderr = result.stderr.trim();
            return Err(VcsError::exit(
                operation,
                "git diff",
                &input.cwd,
                result.exit_code,
                if stderr.is_empty() {
                    "Checkpoint ref is unavailable for diff operation."
                } else {
                    stderr
                },
            ));
        }
        Ok(result.stdout)
    }
}

#[async_trait]
impl VcsCheckpointOps for GitVcsProcessDriver {
    async fn capture_checkpoint(&self, cwd: &str, checkpoint_ref: &str) -> Result<(), VcsError> {
        self.capture(cwd, checkpoint_ref).await
    }

    async fn has_checkpoint_ref(&self, cwd: &str, checkpoint_ref: &str) -> Result<bool, VcsError> {
        Ok(self.resolve_checkpoint_commit(cwd, checkpoint_ref).await?.is_some())
    }

    async fn restore_checkpoint(&self, cwd: &str, checkpoint_ref: &str, fallback_to_head: bool) -> Result<bool, VcsError> {
        self.restore(cwd, checkpoint_ref, fallback_to_head).await
    }

    async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, VcsError> {
        self.diff(input).await
    }

    async fn delete_checkpoint_refs(&self, cwd: &str, checkpoint_refs: &[String]) -> Result<(), VcsError> {
        for checkpoint_ref in checkpoint_refs {
            self.git(
                "GitVcsDriver.checkpoints.deleteCheckpointRefs",
                cwd,
                vec!["update-ref".into(), "-d".into(), checkpoint_ref.clone()],
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    ..Default::default()
                },
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait]
impl VcsDriver for GitVcsProcessDriver {
    fn capabilities(&self) -> VcsDriverCapabilities {
        VcsDriverCapabilities {
            kind: VcsDriverKind::Git,
            supports_worktrees: true,
            supports_bookmarks: false,
            supports_atomic_snapshot: false,
            supports_push_default_remote: true,
            ignore_classifier: "native".into(),
        }
    }

    async fn execute(&self, input: VcsProcessInput) -> Result<VcsProcessOutput, VcsError> {
        let cwd = input.cwd.to_string_lossy().into_owned();
        self.git(
            &input.operation,
            &cwd,
            input.args,
            GitCommandOptions {
                stdin: input.stdin,
                env: input.env,
                allow_non_zero_exit: input.allow_non_zero_exit,
                timeout_ms: input.timeout_ms,
                max_output_bytes: input.max_output_bytes,
                output_mode: input.output_mode,
                append_truncation_marker: input.append_truncation_marker,
            },
        )
        .await
    }

    fn checkpoints(&self) -> Option<&dyn VcsCheckpointOps> {
        Some(self)
    }

    async fn detect_repository(&self, cwd: &str) -> Result<Option<VcsRepositoryIdentity>, VcsError> {
        if !self.is_inside_work_tree(cwd).await? {
            return Ok(None);
        }
        let root = self
            .git(
                "GitVcsDriver.detectRepository.root",
                cwd,
                s(&["rev-parse", "--show-toplevel"]),
                GitCommandOptions::default(),
            )
            .await?;
        let common = self
            .git(
                "GitVcsDriver.detectRepository.commonDir",
                cwd,
                s(&["rev-parse", "--git-common-dir"]),
                GitCommandOptions::default(),
            )
            .await
            .ok();
        let metadata_path = common.map(|c| c.stdout.trim().to_owned()).filter(|p| !p.is_empty());
        Ok(Some(VcsRepositoryIdentity {
            kind: VcsDriverKind::Git,
            root_path: root.stdout.trim().to_owned(),
            metadata_path,
            freshness: VcsFreshness::live_local_now(),
        }))
    }

    async fn is_inside_work_tree(&self, cwd: &str) -> Result<bool, VcsError> {
        let result = self
            .git(
                "GitVcsDriver.isInsideWorkTree",
                cwd,
                s(&["rev-parse", "--is-inside-work-tree"]),
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    timeout_ms: Some(5_000),
                    max_output_bytes: Some(4_096),
                    ..Default::default()
                },
            )
            .await?;
        Ok(result.exit_code == 0 && result.stdout.trim() == "true")
    }

    async fn list_workspace_files(&self, cwd: &str) -> Result<VcsListWorkspaceFilesResult, VcsError> {
        let result = self
            .git(
                "GitVcsDriver.listWorkspaceFiles",
                cwd,
                join(&[
                    &WORKSPACE_GIT_HARDENED_CONFIG_ARGS,
                    &["ls-files", "--cached", "--others", "--exclude-standard", "-z"],
                ]),
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    timeout_ms: Some(20_000),
                    max_output_bytes: Some(WORKSPACE_FILES_MAX_OUTPUT_BYTES),
                    append_truncation_marker: true,
                    ..Default::default()
                },
            )
            .await?;
        if result.exit_code != 0 {
            let stderr = result.stderr.trim();
            return Err(VcsError::exit(
                "GitVcsDriver.listWorkspaceFiles",
                "git ls-files",
                cwd,
                result.exit_code,
                if stderr.is_empty() { "git ls-files failed" } else { stderr },
            ));
        }
        Ok(VcsListWorkspaceFilesResult {
            paths: split_null_separated_paths(&result.stdout, result.stdout_truncated),
            truncated: result.stdout_truncated,
            freshness: VcsFreshness::live_local_now(),
        })
    }

    async fn list_remotes(&self, cwd: &str) -> Result<VcsListRemotesResult, VcsError> {
        let result = self
            .git(
                "GitVcsDriver.listRemotes",
                cwd,
                s(&["remote", "-v"]),
                GitCommandOptions {
                    allow_non_zero_exit: true,
                    timeout_ms: Some(5_000),
                    max_output_bytes: Some(64 * 1024),
                    ..Default::default()
                },
            )
            .await?;
        if result.exit_code != 0 {
            let stderr = result.stderr.trim();
            return Err(VcsError::exit(
                "GitVcsDriver.listRemotes",
                "git remote -v",
                cwd,
                result.exit_code,
                if stderr.is_empty() { "git remote -v failed" } else { stderr },
            ));
        }
        let remotes = parse_git_remote_verbose_output(&result.stdout)
            .into_iter()
            .filter_map(|(name, remote)| {
                let url = remote.url?;
                Some(VcsRemote {
                    is_primary: name == "origin",
                    name,
                    url,
                    push_url: remote.push_url.into(),
                })
            })
            .collect();
        Ok(VcsListRemotesResult {
            remotes,
            freshness: VcsFreshness::live_local_now(),
        })
    }

    async fn filter_ignored_paths(&self, cwd: &str, relative_paths: &[String]) -> Result<Vec<String>, VcsError> {
        if relative_paths.is_empty() {
            return Ok(Vec::new());
        }
        let mut ignored = std::collections::HashSet::new();
        for chunk in chunk_paths_for_git_check_ignore(relative_paths) {
            let result = self
                .git(
                    "GitVcsDriver.filterIgnoredPaths",
                    cwd,
                    join(&[&WORKSPACE_GIT_HARDENED_CONFIG_ARGS, &["check-ignore", "--no-index", "-z", "--stdin"]]),
                    GitCommandOptions {
                        stdin: Some(format!("{}\0", chunk.join("\0"))),
                        allow_non_zero_exit: true,
                        timeout_ms: Some(20_000),
                        max_output_bytes: Some(WORKSPACE_FILES_MAX_OUTPUT_BYTES),
                        append_truncation_marker: true,
                        ..Default::default()
                    },
                )
                .await?;
            if result.exit_code != 0 && result.exit_code != 1 {
                let stderr = result.stderr.trim();
                return Err(VcsError::exit(
                    "GitVcsDriver.filterIgnoredPaths",
                    "git check-ignore",
                    cwd,
                    result.exit_code,
                    if stderr.is_empty() { "git check-ignore failed" } else { stderr },
                ));
            }
            ignored.extend(split_null_separated_paths(&result.stdout, result.stdout_truncated));
        }
        if ignored.is_empty() {
            return Ok(relative_paths.to_vec());
        }
        Ok(relative_paths.iter().filter(|path| !ignored.contains(*path)).cloned().collect())
    }

    async fn init_repository(&self, input: &VcsInitInput) -> Result<(), VcsError> {
        self.git(
            "GitVcsDriver.initRepository",
            &input.cwd,
            s(&["init"]),
            GitCommandOptions {
                timeout_ms: Some(10_000),
                max_output_bytes: Some(64 * 1024),
                ..Default::default()
            },
        )
        .await
        .map(|_| ())
    }
}
