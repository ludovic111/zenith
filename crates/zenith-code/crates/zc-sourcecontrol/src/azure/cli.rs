//! `AzureDevOpsCli.ts`: every `az` invocation (`--only-show-errors --output json` for reads).

use std::sync::OnceLock;

use regex::Regex;
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use zc_contracts::{SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility};
use zc_core::vcs_process::{VcsProcess, VcsProcessError, VcsProcessExitFailureKind, VcsProcessInput, VcsProcessOutput};

use crate::azure::pull_requests::{decode_azure_devops_pull_request_json, decode_azure_devops_pull_request_list_json};
use crate::errors::{error_defect, tagged, Cause, CauseError};
use crate::github::cli::{is_missing_executable, SchemaDecodeError};
use crate::provider::{source_branch, ChangeRequestStateFilter, SourceControlRefSelector};
use crate::records::NormalizedChangeRequest;
use crate::util::{js_length, js_trim, trimmed_non_empty};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The members of the `AzureDevOpsCliError` union.
#[derive(Debug, Clone, PartialEq)]
pub enum AzureDevOpsCliErrorKind {
    Unavailable {
        argument_count: usize,
    },
    Authentication {
        argument_count: usize,
    },
    RateLimit {
        argument_count: usize,
    },
    PullRequestNotFound {
        argument_count: usize,
    },
    CommandFailed {
        argument_count: usize,
    },
    PullRequestListDecode {
        output_length: usize,
    },
    PullRequestDecode {
        output_length: usize,
    },
    /// `operation` is `getRepositoryCloneUrls`, `getDefaultBranch` or `createRepository`.
    RepositoryDecode {
        operation: &'static str,
        output_length: usize,
    },
}

/// `AzureDevOpsCliError`.
#[derive(Debug, Clone)]
pub struct AzureDevOpsCliError {
    pub kind: AzureDevOpsCliErrorKind,
    pub cwd: String,
    pub cause: Cause,
}

impl AzureDevOpsCliError {
    pub fn command(&self) -> &'static str {
        "az"
    }

    pub fn tag(&self) -> &'static str {
        use AzureDevOpsCliErrorKind::*;
        match self.kind {
            Unavailable { .. } => "AzureDevOpsCliUnavailableError",
            Authentication { .. } => "AzureDevOpsCliAuthenticationError",
            RateLimit { .. } => "AzureDevOpsCliRateLimitError",
            PullRequestNotFound { .. } => "AzureDevOpsPullRequestNotFoundError",
            CommandFailed { .. } => "AzureDevOpsCommandFailedError",
            PullRequestListDecode { .. } => "AzureDevOpsPullRequestListDecodeError",
            PullRequestDecode { .. } => "AzureDevOpsPullRequestDecodeError",
            RepositoryDecode { .. } => "AzureDevOpsRepositoryDecodeError",
        }
    }

    pub fn operation(&self) -> &'static str {
        use AzureDevOpsCliErrorKind::*;
        match self.kind {
            PullRequestListDecode { .. } => "listPullRequests",
            PullRequestDecode { .. } => "getPullRequest",
            RepositoryDecode { operation, .. } => operation,
            _ => "execute",
        }
    }

    pub fn detail(&self) -> &'static str {
        use AzureDevOpsCliErrorKind::*;
        match self.kind {
            Unavailable { .. } => "Azure CLI (`az`) with the Azure DevOps extension is required but not available on PATH.",
            Authentication { .. } => "Azure DevOps CLI is not authenticated. Run `az devops login` and retry.",
            RateLimit { .. } => "Azure DevOps API rate limit exceeded.",
            PullRequestNotFound { .. } => "Pull request not found. Check the PR number or URL and try again.",
            CommandFailed { .. } => "Azure DevOps CLI command failed.",
            PullRequestListDecode { .. } => "Azure DevOps CLI returned invalid PR list JSON.",
            PullRequestDecode { .. } => "Azure DevOps CLI returned invalid pull request JSON.",
            RepositoryDecode { .. } => "Azure DevOps CLI returned invalid repository JSON.",
        }
    }

    pub fn message(&self) -> String {
        format!("Azure DevOps CLI failed in {}: {}", self.operation(), self.detail())
    }

    /// `AzureDevOpsCommandFailedError.fromVcsError`.
    pub fn from_vcs_error(cwd: &str, argument_count: usize, error: VcsProcessError) -> Self {
        use AzureDevOpsCliErrorKind::*;
        let kind = match &error {
            VcsProcessError::Spawn { cause, .. } if is_missing_executable(cwd, &cause.message()) => Unavailable { argument_count },
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::Authentication),
                ..
            } => Authentication { argument_count },
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::RateLimited),
                ..
            } => RateLimit { argument_count },
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::NotFound),
                ..
            } => PullRequestNotFound { argument_count },
            _ => CommandFailed { argument_count },
        };
        Self {
            kind,
            cwd: cwd.to_owned(),
            cause: Cause::new(error),
        }
    }

    fn decode(kind: AzureDevOpsCliErrorKind, cwd: &str, error: String) -> Self {
        Self {
            kind,
            cwd: cwd.to_owned(),
            cause: Cause::new(SchemaDecodeError(error)),
        }
    }
}

impl std::fmt::Display for AzureDevOpsCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for AzureDevOpsCliError {}

impl Serialize for AzureDevOpsCliError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use AzureDevOpsCliErrorKind::*;
        let mut fields = json!({"operation": self.operation(), "command": "az", "cwd": self.cwd});
        match self.kind {
            Unavailable { argument_count }
            | Authentication { argument_count }
            | RateLimit { argument_count }
            | PullRequestNotFound { argument_count }
            | CommandFailed { argument_count } => fields["argumentCount"] = json!(argument_count),
            PullRequestListDecode { output_length } | PullRequestDecode { output_length } | RepositoryDecode { output_length, .. } => {
                fields["outputLength"] = json!(output_length)
            }
        }
        tagged(self.tag(), fields, Some(&self.cause)).serialize(serializer)
    }
}

impl CauseError for AzureDevOpsCliError {
    fn defect(&self) -> Value {
        error_defect(self.tag(), self.message(), Some(self.cause.defect()))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `normalizeChangeRequestId`: `#12`, `12` or a pull request URL → `12`.
pub fn normalize_change_request_id(reference: &str) -> String {
    static URL: OnceLock<Regex> = OnceLock::new();
    let trimmed = js_trim(reference);
    let trimmed = trimmed.strip_prefix('#').unwrap_or(trimmed);
    let pattern = URL.get_or_init(|| Regex::new(r"(?i)(?:pullrequest|pull-request|pull|_pulls?)/([0-9]+)(?:[^0-9].*)?$").expect("valid regex"));
    pattern
        .captures(trimmed)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
        .unwrap_or_else(|| trimmed.to_owned())
}

fn to_azure_status(state: ChangeRequestStateFilter) -> &'static str {
    match state {
        ChangeRequestStateFilter::Open => "active",
        ChangeRequestStateFilter::Closed => "abandoned",
        ChangeRequestStateFilter::Merged => "completed",
        ChangeRequestStateFilter::All => "all",
    }
}

struct RawRepository {
    name: String,
    remote_url: String,
    ssh_url: String,
    project_name: Option<String>,
    default_branch: Option<String>,
}

fn decode_raw_repository(raw: &str) -> Result<RawRepository, String> {
    let value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let map = value.as_object().ok_or("Expected an object")?;
    let field = |key: &str| map.get(key).and_then(trimmed_non_empty).ok_or_else(|| format!("Missing or invalid {key}"));
    let name = field("name")?;
    field("webUrl")?;
    let remote_url = field("remoteUrl")?;
    let ssh_url = field("sshUrl")?;
    let project_name = match map.get("project") {
        None => None,
        Some(Value::Object(project)) => Some(project.get("name").and_then(trimmed_non_empty).ok_or("Missing project.name")?),
        Some(_) => return Err("Invalid project".into()),
    };
    let default_branch = match map.get("defaultBranch") {
        None | Some(Value::Null) => None,
        Some(Value::String(branch)) => Some(branch.clone()),
        Some(_) => return Err("Invalid defaultBranch".into()),
    };
    Ok(RawRepository {
        name,
        remote_url,
        ssh_url,
        project_name,
        default_branch,
    })
}

fn normalize_default_branch(value: Option<&str>) -> Option<String> {
    let trimmed = js_trim(value.unwrap_or_default());
    let trimmed = trimmed.strip_prefix("refs/heads/").unwrap_or(trimmed);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn clone_urls(raw: &RawRepository) -> SourceControlRepositoryCloneUrls {
    let project = raw.project_name.as_deref().map(js_trim).filter(|p| !p.is_empty());
    SourceControlRepositoryCloneUrls {
        name_with_owner: match project {
            Some(project) => format!("{project}/{}", raw.name),
            None => raw.name.clone(),
        },
        url: raw.remote_url.clone(),
        ssh_url: raw.ssh_url.clone(),
    }
}

/// `parseRepositorySpecifier`: `[org/]project/name`.
fn parse_repository_specifier(repository: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = repository.split('/').map(js_trim).filter(|p| !p.is_empty()).collect();
    let project = (parts.len() > 1).then(|| parts[parts.len() - 2].to_owned());
    let name = parts.last().map_or_else(|| js_trim(repository).to_owned(), |p| (*p).to_owned());
    (project, name)
}

/// `execute` input.
#[derive(Debug, Clone, Default)]
pub struct AzureExecuteInput {
    pub cwd: String,
    pub args: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub max_output_bytes: Option<usize>,
}

impl AzureExecuteInput {
    pub fn new<I, S>(cwd: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            cwd: cwd.into(),
            args: args.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }
}

/// The `AzureDevOpsCli` service.
#[derive(Clone)]
pub struct AzureDevOpsCli {
    process: VcsProcess,
}

impl AzureDevOpsCli {
    pub fn new(process: VcsProcess) -> Self {
        Self { process }
    }

    /// `execute`.
    pub async fn execute(&self, input: AzureExecuteInput) -> Result<VcsProcessOutput, AzureDevOpsCliError> {
        let mut run = VcsProcessInput::new("AzureDevOpsCli.execute", "az", input.args.iter().cloned(), input.cwd.as_str());
        run.timeout_ms = Some(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        run.max_output_bytes = input.max_output_bytes;
        self.process
            .run(run)
            .await
            .map_err(|error| AzureDevOpsCliError::from_vcs_error(&input.cwd, input.args.len(), error))
    }

    async fn execute_json(&self, mut input: AzureExecuteInput) -> Result<String, AzureDevOpsCliError> {
        input.args.extend(["--only-show-errors", "--output", "json"].map(String::from));
        let output = self.execute(input).await?;
        Ok(js_trim(&output.stdout).to_owned())
    }

    /// `listPullRequests`.
    pub async fn list_pull_requests(
        &self,
        cwd: &str,
        head_selector: &str,
        source: Option<&SourceControlRefSelector>,
        state: ChangeRequestStateFilter,
        limit: Option<u32>,
    ) -> Result<Vec<NormalizedChangeRequest>, AzureDevOpsCliError> {
        let raw = self
            .execute_json(AzureExecuteInput::new(
                cwd,
                [
                    "repos".to_owned(),
                    "pr".into(),
                    "list".into(),
                    "--detect".into(),
                    "true".into(),
                    "--source-branch".into(),
                    source_branch(head_selector, source),
                    "--status".into(),
                    to_azure_status(state).into(),
                    "--top".into(),
                    limit.unwrap_or(20).to_string(),
                ],
            ))
            .await?;
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        decode_azure_devops_pull_request_list_json(&raw).map_err(|e| {
            AzureDevOpsCliError::decode(
                AzureDevOpsCliErrorKind::PullRequestListDecode {
                    output_length: js_length(&raw),
                },
                cwd,
                e,
            )
        })
    }

    /// `getPullRequest`.
    pub async fn get_pull_request(&self, cwd: &str, reference: &str) -> Result<NormalizedChangeRequest, AzureDevOpsCliError> {
        let raw = self
            .execute_json(AzureExecuteInput::new(
                cwd,
                [
                    "repos".to_owned(),
                    "pr".into(),
                    "show".into(),
                    "--detect".into(),
                    "true".into(),
                    "--id".into(),
                    normalize_change_request_id(reference),
                ],
            ))
            .await?;
        decode_azure_devops_pull_request_json(&raw).map_err(|e| {
            AzureDevOpsCliError::decode(
                AzureDevOpsCliErrorKind::PullRequestDecode {
                    output_length: js_length(&raw),
                },
                cwd,
                e,
            )
        })
    }

    fn repository(raw: &str, operation: &'static str, cwd: &str) -> Result<RawRepository, AzureDevOpsCliError> {
        decode_raw_repository(raw).map_err(|e| {
            AzureDevOpsCliError::decode(
                AzureDevOpsCliErrorKind::RepositoryDecode {
                    operation,
                    output_length: js_length(raw),
                },
                cwd,
                e,
            )
        })
    }

    /// `getRepositoryCloneUrls`.
    pub async fn get_repository_clone_urls(&self, cwd: &str, repository: &str) -> Result<SourceControlRepositoryCloneUrls, AzureDevOpsCliError> {
        let raw = self
            .execute_json(AzureExecuteInput::new(cwd, ["repos", "show", "--detect", "true", "--repository", repository]))
            .await?;
        Ok(clone_urls(&Self::repository(&raw, "getRepositoryCloneUrls", cwd)?))
    }

    /// `createRepository`. `az repos create` has no per-repository visibility (Azure Repos access
    /// follows project permissions), so `visibility` is intentionally not translated.
    pub async fn create_repository(
        &self,
        cwd: &str,
        repository: &str,
        _visibility: SourceControlRepositoryVisibility,
    ) -> Result<SourceControlRepositoryCloneUrls, AzureDevOpsCliError> {
        let (project, name) = parse_repository_specifier(repository);
        let mut args = vec!["repos".to_owned(), "create".into(), "--detect".into(), "true".into(), "--name".into(), name];
        if let Some(project) = project {
            args.extend(["--project".to_owned(), project]);
        }
        let raw = self.execute_json(AzureExecuteInput::new(cwd, args)).await?;
        Ok(clone_urls(&Self::repository(&raw, "createRepository", cwd)?))
    }

    /// `createPullRequest`: the description is read by `az` from the body file (`@file`).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_pull_request(
        &self,
        cwd: &str,
        base_branch: &str,
        head_selector: &str,
        source: Option<&SourceControlRefSelector>,
        target: Option<&SourceControlRefSelector>,
        title: &str,
        body_file: &str,
    ) -> Result<(), AzureDevOpsCliError> {
        self.execute(AzureExecuteInput::new(
            cwd,
            [
                "repos".to_owned(),
                "pr".into(),
                "create".into(),
                "--only-show-errors".into(),
                "--detect".into(),
                "true".into(),
                "--target-branch".into(),
                target.map_or(base_branch, |t| t.ref_name.as_str()).to_owned(),
                "--source-branch".into(),
                source_branch(head_selector, source),
                "--title".into(),
                title.to_owned(),
                "--description".into(),
                format!("@{body_file}"),
            ],
        ))
        .await
        .map(drop)
    }

    /// `getDefaultBranch`.
    pub async fn get_default_branch(&self, cwd: &str) -> Result<Option<String>, AzureDevOpsCliError> {
        let raw = self.execute_json(AzureExecuteInput::new(cwd, ["repos", "show", "--detect", "true"])).await?;
        Ok(normalize_default_branch(
            Self::repository(&raw, "getDefaultBranch", cwd)?.default_branch.as_deref(),
        ))
    }

    /// `checkoutPullRequest`.
    pub async fn checkout_pull_request(&self, cwd: &str, reference: &str, remote_name: Option<&str>) -> Result<(), AzureDevOpsCliError> {
        self.execute(AzureExecuteInput::new(
            cwd,
            [
                "repos".to_owned(),
                "pr".into(),
                "checkout".into(),
                "--only-show-errors".into(),
                "--detect".into(),
                "true".into(),
                "--id".into(),
                normalize_change_request_id(reference),
                "--remote-name".into(),
                remote_name.unwrap_or("origin").to_owned(),
            ],
        ))
        .await
        .map(drop)
    }
}
