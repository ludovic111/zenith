//! `SourceControlRepositoryService.ts`: look up a repository on a forge, clone it (with progress
//! parsed from git's stderr), discard a failed clone, publish a local repository.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use regex::Regex;
use zc_contracts::{
    SourceControlCloneProtocol, SourceControlCloneRepositoryInput, SourceControlCloneRepositoryResult, SourceControlProviderKind,
    SourceControlPublishRepositoryInput, SourceControlPublishRepositoryResult, SourceControlPublishStatus, SourceControlRepositoryCloneUrls,
    SourceControlRepositoryInfo, SourceControlRepositoryLookupInput,
};
use zc_vcs::git_exec::{env, ExecuteGitInput, ExecuteGitProgress, GitTimeout};
use zc_vcs::GitVcsDriver;

use crate::clone_progress::{parse_git_clone_progress_line, GitCloneProgressLine};
use crate::errors::{Cause, SourceControlRepositoryError};
use crate::provider::{CreateRepositoryInput, RepositoryCloneUrlsInput};
use crate::registry::SourceControlProviderRegistry;
use crate::util::{js_trim, parse_url};

/// The synchronous RPC (older clients, mobile) keeps a deadline: nothing else tells the user a
/// clone stalled. The tracked path passes [`CloneTimeout::None`] and relies on progress and
/// cancellation instead.
pub const CLONE_TIMEOUT_MS: u64 = 120_000;
const GENERIC_DETAIL: &str = "The source control operation could not be completed.";

/// `SourceControlPreparedClone`.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceControlPreparedClone {
    pub destination_path: String,
    /// Credential-free; safe to show and to store in snapshots.
    pub remote_url: String,
    /// What git is given; may carry embedded credentials.
    pub clone_url: String,
    pub repository: Option<SourceControlRepositoryInfo>,
}

/// The clone deadline (`timeoutMs`: absent, a number, or `null`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloneTimeout {
    /// 120 s.
    #[default]
    Default,
    Millis(u64),
    None,
}

pub type CloneProgressCallback = Arc<dyn Fn(GitCloneProgressLine) + Send + Sync>;

/// `SourceControlCloneOptions`.
#[derive(Clone, Default)]
pub struct SourceControlCloneOptions {
    pub on_progress: Option<CloneProgressCallback>,
    pub timeout: CloneTimeout,
}

/// A failure inside an operation, before `mapRepositoryError`: repository errors pass through,
/// anything else becomes the generic detail with the cause kept.
enum Failure {
    Repository(SourceControlRepositoryError),
    Other(Cause),
}

impl From<SourceControlRepositoryError> for Failure {
    fn from(error: SourceControlRepositoryError) -> Self {
        Self::Repository(error)
    }
}

impl From<crate::errors::SourceControlProviderError> for Failure {
    fn from(error: crate::errors::SourceControlProviderError) -> Self {
        Self::Other(Cause::new(error))
    }
}

fn map_failure(operation: &str, provider: SourceControlProviderKind, failure: Failure) -> SourceControlRepositoryError {
    match failure {
        Failure::Repository(error) => error,
        Failure::Other(cause) => SourceControlRepositoryError::new(operation, provider, GENERIC_DETAIL).with_cause(cause),
    }
}

fn io_failure(error: std::io::Error) -> Failure {
    Failure::Other(Cause::new(error))
}

fn to_repository_info(provider: SourceControlProviderKind, urls: &SourceControlRepositoryCloneUrls) -> SourceControlRepositoryInfo {
    SourceControlRepositoryInfo {
        provider,
        name_with_owner: urls.name_with_owner.clone(),
        url: urls.url.clone(),
        ssh_url: urls.ssh_url.clone(),
    }
}

/// `redactRemoteUrl`: the URL clients see, without userinfo or query (a pasted
/// `https://user:token@host/…` must not travel back to every reader; git still gets the original).
pub fn redact_remote_url(remote_url: &str) -> String {
    let Some(mut url) = parse_url(remote_url) else {
        return remote_url.to_owned();
    };
    if url.username().is_empty() && url.password().is_none() && url.query().is_none_or(str::is_empty) {
        return remote_url.to_owned();
    }
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.to_string()
}

/// `redactUrlCredentials`: drops `user:token@` from any URL embedded in free text.
pub fn redact_url_credentials(text: &str) -> String {
    static USERINFO: OnceLock<Regex> = OnceLock::new();
    USERINFO
        .get_or_init(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^\s/]+@").expect("valid regex"))
        .replace_all(text, "$1")
        .into_owned()
}

fn select_remote_url(urls: &SourceControlRepositoryCloneUrls, protocol: Option<SourceControlCloneProtocol>) -> String {
    match protocol.unwrap_or(SourceControlCloneProtocol::Auto) {
        SourceControlCloneProtocol::Https => urls.url.clone(),
        SourceControlCloneProtocol::Ssh | SourceControlCloneProtocol::Auto => urls.ssh_url.clone(),
    }
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The `SourceControlRepositoryService`.
#[derive(Clone)]
pub struct SourceControlRepositoryService {
    cwd: String,
    git: GitVcsDriver,
    providers: SourceControlProviderRegistry,
}

impl SourceControlRepositoryService {
    /// `cwd` is the server's working directory, used for lookups without one.
    pub fn new(cwd: impl Into<String>, git: GitVcsDriver, providers: SourceControlProviderRegistry) -> Self {
        Self {
            cwd: cwd.into(),
            git,
            providers,
        }
    }

    fn ensure_concrete_provider(operation: &str, provider: SourceControlProviderKind) -> Result<SourceControlProviderKind, SourceControlRepositoryError> {
        if provider != SourceControlProviderKind::Unknown {
            return Ok(provider);
        }
        Err(SourceControlRepositoryError::new(
            operation,
            provider,
            "Choose a source control provider before continuing.",
        ))
    }

    async fn lookup(&self, input: &SourceControlRepositoryLookupInput) -> Result<SourceControlRepositoryInfo, Failure> {
        let kind = Self::ensure_concrete_provider("lookupRepository", input.provider)?;
        let urls = self
            .providers
            .get(kind)
            .get_repository_clone_urls(RepositoryCloneUrlsInput {
                cwd: input.cwd.clone().unwrap_or_else(|| self.cwd.clone()),
                context: None,
                repository: js_trim(&input.repository).to_owned(),
            })
            .await?;
        Ok(to_repository_info(kind, &urls))
    }

    /// `lookupRepository`.
    pub async fn lookup_repository(&self, input: &SourceControlRepositoryLookupInput) -> Result<SourceControlRepositoryInfo, SourceControlRepositoryError> {
        self.lookup(input).await.map_err(|f| map_failure("lookupRepository", input.provider, f))
    }

    fn normalize_destination_path(destination_path: &str) -> Result<PathBuf, SourceControlRepositoryError> {
        let trimmed = js_trim(destination_path);
        if trimmed.is_empty() {
            return Err(SourceControlRepositoryError::new(
                "cloneRepository",
                SourceControlProviderKind::Unknown,
                "Choose a destination path before cloning.",
            ));
        }
        Ok(zc_core::paths::resolve_path(&zc_core::paths::expand_home_path(trimmed)))
    }

    async fn prepare_destination(destination_path: &str) -> Result<PathBuf, Failure> {
        let destination = Self::normalize_destination_path(destination_path)?;
        if tokio::fs::try_exists(&destination).await.map_err(io_failure)? {
            let mut entries = tokio::fs::read_dir(&destination).await.map_err(|cause| {
                SourceControlRepositoryError::new(
                    "cloneRepository",
                    SourceControlProviderKind::Unknown,
                    "Destination path already exists and is not a directory.",
                )
                .with_cause(Cause::new(cause))
            })?;
            if entries.next_entry().await.map_err(io_failure)?.is_some() {
                return Err(SourceControlRepositoryError::new(
                    "cloneRepository",
                    SourceControlProviderKind::Unknown,
                    "Destination path already exists and is not empty.",
                )
                .into());
            }
        } else if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(io_failure)?;
        }
        Ok(destination)
    }

    async fn prepare(&self, input: &SourceControlCloneRepositoryInput) -> Result<SourceControlPreparedClone, Failure> {
        let destination = Self::prepare_destination(&input.destination_path).await?;
        let parent = destination.parent().map(path_string).unwrap_or_else(|| "/".into());
        let mut repository = None;
        let mut remote_url = input.remote_url.as_deref().map(|u| js_trim(u).to_owned()).filter(|u| !u.is_empty());
        let mut provider = input.provider.unwrap_or(SourceControlProviderKind::Unknown);
        if let (Some(kind), Some(name)) = (input.provider, input.repository.as_ref()) {
            let info = self
                .lookup(&SourceControlRepositoryLookupInput {
                    provider: kind,
                    repository: name.clone(),
                    cwd: Some(parent),
                })
                .await?;
            remote_url = Some(select_remote_url(
                &SourceControlRepositoryCloneUrls {
                    name_with_owner: info.name_with_owner.clone(),
                    url: info.url.clone(),
                    ssh_url: info.ssh_url.clone(),
                },
                input.protocol,
            ));
            repository = Some(info);
            provider = kind;
        }
        let Some(remote_url) = remote_url else {
            return Err(SourceControlRepositoryError::new("cloneRepository", provider, "Enter a repository path or clone URL before cloning.").into());
        };
        Ok(SourceControlPreparedClone {
            destination_path: path_string(&destination),
            remote_url: redact_remote_url(&remote_url),
            clone_url: remote_url,
            repository,
        })
    }

    /// `prepareClone`: everything `clone_repository` checks before running git (the resolved
    /// remote, the normalized destination, that it is empty), so a caller can create the project
    /// first and clone afterwards.
    pub async fn prepare_clone(&self, input: &SourceControlCloneRepositoryInput) -> Result<SourceControlPreparedClone, SourceControlRepositoryError> {
        self.prepare(input)
            .await
            .map_err(|f| map_failure("cloneRepository", input.provider.unwrap_or(SourceControlProviderKind::Unknown), f))
    }

    async fn clone(
        &self,
        input: &SourceControlCloneRepositoryInput,
        options: &SourceControlCloneOptions,
    ) -> Result<SourceControlCloneRepositoryResult, Failure> {
        let prepared = self.prepare(input).await?;
        let destination = PathBuf::from(&prepared.destination_path);
        let parent = destination.parent().map(path_string).unwrap_or_else(|| "/".into());
        let directory = destination.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // Git interleaves progress redraws with its real messages on stderr. The last
        // non-progress lines explain a failure ("Repository not found", …), so keep them.
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let on_progress = options.on_progress.clone();
        let stderr_tail = tail.clone();
        let on_stderr_line: zc_vcs::git_exec::LineCallback = Arc::new(move |line: &str| {
            if let Some(parsed) = parse_git_clone_progress_line(line) {
                if let Some(callback) = &on_progress {
                    callback(parsed);
                }
                return;
            }
            let trimmed = js_trim(line);
            if trimmed.is_empty() || trimmed.starts_with("Cloning into") {
                return;
            }
            // Git echoes the remote in some failures; the tail becomes user-facing text.
            let mut tail = stderr_tail.lock().expect("stderr tail lock");
            tail.push_back(redact_url_credentials(trimmed));
            if tail.len() > 4 {
                tail.pop_front();
            }
        });
        let mut execute = ExecuteGitInput::new(
            "SourceControlRepositoryService.cloneRepository",
            &parent,
            ["clone".to_owned(), "--progress".into(), prepared.clone_url.clone(), directory],
        );
        execute.timeout = match options.timeout {
            CloneTimeout::Default => GitTimeout::Millis(CLONE_TIMEOUT_MS),
            CloneTimeout::Millis(ms) => GitTimeout::Millis(ms),
            CloneTimeout::None => GitTimeout::Unbounded,
        };
        // Progress redraws add up on a slow multi-GB clone. The buffered copy is never read (the
        // tail is kept above), so keep it small and let the line callbacks flow past the cap.
        execute.max_output_bytes = Some(256 * 1024);
        execute.append_truncation_marker = true;
        execute.keep_line_callbacks_after_truncation = true;
        execute.env = Some(env([("GIT_PROGRESS_DELAY", "0"), ("GIT_TERMINAL_PROMPT", "0"), ("LC_ALL", "C")]));
        execute.progress = Some(ExecuteGitProgress {
            on_stderr_line: Some(on_stderr_line),
            ..ExecuteGitProgress::default()
        });
        if let Err(cause) = self.git.execute(execute).await {
            let tail = tail.lock().expect("stderr tail lock");
            let detail = if tail.is_empty() {
                "The repository could not be cloned.".to_owned()
            } else {
                tail.iter().cloned().collect::<Vec<_>>().join(" ")
            };
            return Err(
                SourceControlRepositoryError::new("cloneRepository", input.provider.unwrap_or(SourceControlProviderKind::Unknown), detail)
                    .with_cause(Cause::new(cause))
                    .into(),
            );
        }
        Ok(SourceControlCloneRepositoryResult {
            cwd: prepared.destination_path,
            remote_url: prepared.remote_url,
            repository: prepared.repository,
        })
    }

    /// `cloneRepository`.
    pub async fn clone_repository(
        &self,
        input: &SourceControlCloneRepositoryInput,
        options: &SourceControlCloneOptions,
    ) -> Result<SourceControlCloneRepositoryResult, SourceControlRepositoryError> {
        self.clone(input, options)
            .await
            .map_err(|f| map_failure("cloneRepository", input.provider.unwrap_or(SourceControlProviderKind::Unknown), f))
    }

    /// `discardClone`: empties what git left behind, keeping the directory (the project's
    /// workspace root). Refuses when something other than a clone is there.
    pub async fn discard_clone(&self, destination_path: &str) -> Result<(), SourceControlRepositoryError> {
        let unknown = SourceControlProviderKind::Unknown;
        // Like TS, an empty path reports the `cloneRepository` operation.
        let normalized = Self::normalize_destination_path(destination_path)?;
        let mut names = Vec::new();
        match tokio::fs::read_dir(&normalized).await {
            Ok(mut entries) => loop {
                match entries.next_entry().await {
                    Ok(Some(entry)) => names.push(entry.file_name().to_string_lossy().into_owned()),
                    Ok(None) => break,
                    Err(cause) => {
                        return Err(
                            SourceControlRepositoryError::new("discardClone", unknown, "The clone destination could not be inspected.")
                                .with_cause(Cause::new(cause)),
                        )
                    }
                }
            },
            Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {}
            Err(cause) => {
                return Err(
                    SourceControlRepositoryError::new("discardClone", unknown, "The clone destination could not be inspected.").with_cause(Cause::new(cause)),
                )
            }
        }
        if !names.is_empty() && !names.iter().any(|name| name == ".git") {
            return Err(SourceControlRepositoryError::new(
                "discardClone",
                unknown,
                "Destination path contains files that are not from the clone.",
            ));
        }
        // An interrupted git may still be closing files, so removal retries briefly.
        let mut attempt = 0;
        loop {
            let result = async {
                match tokio::fs::remove_dir_all(&normalized).await {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                tokio::fs::create_dir_all(&normalized).await
            }
            .await;
            match result {
                Ok(()) => return Ok(()),
                Err(_) if attempt < 5 => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Err(cause) => {
                    return Err(
                        SourceControlRepositoryError::new("discardClone", unknown, "The partial clone could not be removed.").with_cause(Cause::new(cause)),
                    )
                }
            }
        }
    }

    async fn publish(&self, input: &SourceControlPublishRepositoryInput) -> Result<SourceControlPublishRepositoryResult, Failure> {
        let kind = Self::ensure_concrete_provider("publishRepository", input.provider)?;
        let urls = self
            .providers
            .get(kind)
            .create_repository(CreateRepositoryInput {
                cwd: input.cwd.clone(),
                repository: js_trim(&input.repository).to_owned(),
                visibility: input.visibility,
            })
            .await?;
        let remote_url = select_remote_url(&urls, input.protocol);
        let preferred = input.remote_name.as_deref().map(js_trim).filter(|n| !n.is_empty()).unwrap_or("origin");
        let remote_name = self
            .git
            .ensure_remote(&input.cwd, preferred, &remote_url)
            .await
            .map_err(|e| Failure::Other(Cause::new(e)))?;
        // An empty local repository (no commits) would make the push fail with an opaque
        // "src refspec HEAD does not match any": the remote is created and wired up, but there
        // is nothing to push yet.
        let has_commits = self
            .git
            .execute(ExecuteGitInput::new(
                "SourceControlRepositoryService.publishRepository.headCheck",
                &input.cwd,
                ["rev-parse", "--verify", "HEAD"],
            ))
            .await
            .is_ok();
        if !has_commits {
            let branch = self
                .git
                .status_details(&input.cwd)
                .await
                .ok()
                .and_then(|d| d.branch)
                .unwrap_or_else(|| "main".into());
            return Ok(SourceControlPublishRepositoryResult {
                repository: to_repository_info(kind, &urls),
                remote_name,
                remote_url,
                branch,
                upstream_branch: None,
                status: SourceControlPublishStatus::RemoteAdded,
            });
        }
        let push = self
            .git
            .push_current_branch(&input.cwd, None, Some(&remote_name))
            .await
            .map_err(|e| Failure::Other(Cause::new(e)))?;
        Ok(SourceControlPublishRepositoryResult {
            repository: to_repository_info(kind, &urls),
            remote_name,
            remote_url,
            branch: push.branch,
            upstream_branch: push.upstream_branch.filter(|b| !b.is_empty()),
            status: SourceControlPublishStatus::Pushed,
        })
    }

    /// `publishRepository`: create the repository on the forge, add (or reuse) the remote, push
    /// the current branch with upstream.
    pub async fn publish_repository(
        &self,
        input: &SourceControlPublishRepositoryInput,
    ) -> Result<SourceControlPublishRepositoryResult, SourceControlRepositoryError> {
        self.publish(input).await.map_err(|f| map_failure("publishRepository", input.provider, f))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_reported_urls() {
        assert_eq!(
            redact_remote_url("https://user:s3cret@github.com/octocat/demo.git"),
            "https://github.com/octocat/demo.git"
        );
        assert_eq!(
            redact_remote_url("https://github.com/octocat/demo.git?access_token=s3cret"),
            "https://github.com/octocat/demo.git"
        );
        assert_eq!(
            redact_remote_url("https://user:pa@rt@github.com/octocat/demo.git"),
            "https://github.com/octocat/demo.git"
        );
        assert_eq!(redact_remote_url("git@github.com:octocat/demo.git"), "git@github.com:octocat/demo.git");
        assert_eq!(
            redact_url_credentials("fatal: unable to access 'https://user:s3c@ret@github.com/octocat/demo.git/': could not resolve host"),
            "fatal: unable to access 'https://github.com/octocat/demo.git/': could not resolve host"
        );
    }
}
