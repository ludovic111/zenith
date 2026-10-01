//! `GitLabCli.ts`: every `glab` invocation.

use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use zc_contracts::{SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility};
use zc_core::vcs_process::{VcsProcess, VcsProcessError, VcsProcessExitFailureKind, VcsProcessInput, VcsProcessOutput};

use crate::errors::{error_defect, tagged, Cause, CauseError};
use crate::github::cli::SchemaDecodeError;
use crate::gitlab::merge_requests::{decode_gitlab_merge_request_json, decode_gitlab_merge_request_list_json};
use crate::provider::{ChangeRequestStateFilter, SourceControlRefSelector};
use crate::records::NormalizedChangeRequest;
use crate::util::{encode_uri_component, js_trim, trimmed_non_empty};

const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The members of the `GitLabCliError` union.
#[derive(Debug, Clone, PartialEq)]
pub enum GitLabCliErrorKind {
    Unavailable,
    Authentication,
    RateLimit,
    MergeRequestNotFound {
        reference: String,
    },
    Command,
    MergeRequestListDecode,
    MergeRequestDecode {
        reference: String,
    },
    /// `operation` is `getRepositoryCloneUrls`, `createRepository` or `getDefaultBranch`.
    RepositoryDecode {
        operation: &'static str,
        repository: Option<String>,
    },
    NamespaceDecode {
        namespace_path: String,
    },
}

/// `GitLabCliError`.
#[derive(Debug, Clone)]
pub struct GitLabCliError {
    pub kind: GitLabCliErrorKind,
    pub cwd: String,
    pub cause: Cause,
}

impl GitLabCliError {
    pub fn new(kind: GitLabCliErrorKind, cwd: impl Into<String>, cause: Cause) -> Self {
        Self { kind, cwd: cwd.into(), cause }
    }

    pub fn command(&self) -> &'static str {
        "glab"
    }

    pub fn tag(&self) -> &'static str {
        match self.kind {
            GitLabCliErrorKind::Unavailable => "GitLabCliUnavailableError",
            GitLabCliErrorKind::Authentication => "GitLabCliAuthenticationError",
            GitLabCliErrorKind::RateLimit => "GitLabCliRateLimitError",
            GitLabCliErrorKind::MergeRequestNotFound { .. } => "GitLabMergeRequestNotFoundError",
            GitLabCliErrorKind::Command => "GitLabCliCommandError",
            GitLabCliErrorKind::MergeRequestListDecode => "GitLabMergeRequestListDecodeError",
            GitLabCliErrorKind::MergeRequestDecode { .. } => "GitLabMergeRequestDecodeError",
            GitLabCliErrorKind::RepositoryDecode { .. } => "GitLabRepositoryDecodeError",
            GitLabCliErrorKind::NamespaceDecode { .. } => "GitLabNamespaceDecodeError",
        }
    }

    pub fn operation(&self) -> &'static str {
        match &self.kind {
            GitLabCliErrorKind::MergeRequestListDecode => "listMergeRequests",
            GitLabCliErrorKind::MergeRequestDecode { .. } => "getMergeRequest",
            GitLabCliErrorKind::RepositoryDecode { operation, .. } => operation,
            GitLabCliErrorKind::NamespaceDecode { .. } => "createRepository",
            _ => "execute",
        }
    }

    pub fn detail(&self) -> String {
        match &self.kind {
            GitLabCliErrorKind::Unavailable => "GitLab CLI (`glab`) is required but not available on PATH.".into(),
            GitLabCliErrorKind::Authentication => "GitLab CLI is not authenticated. Run `glab auth login` and retry.".into(),
            GitLabCliErrorKind::RateLimit => "GitLab API rate limit exceeded.".into(),
            GitLabCliErrorKind::MergeRequestNotFound { reference } => {
                format!("Merge request {reference} was not found. Check the MR number or URL and try again.")
            }
            GitLabCliErrorKind::Command => "GitLab CLI command failed.".into(),
            GitLabCliErrorKind::MergeRequestListDecode => "GitLab CLI returned invalid MR list JSON.".into(),
            GitLabCliErrorKind::MergeRequestDecode { .. } => "GitLab CLI returned invalid merge request JSON.".into(),
            GitLabCliErrorKind::RepositoryDecode { .. } => "GitLab CLI returned invalid repository JSON.".into(),
            GitLabCliErrorKind::NamespaceDecode { .. } => "GitLab CLI returned invalid namespace JSON.".into(),
        }
    }

    pub fn message(&self) -> String {
        format!("GitLab CLI failed in {}: {}", self.operation(), self.detail())
    }

    /// `GitLabCliCommandError.fromVcsError`.
    pub fn from_vcs_error(cwd: &str, error: VcsProcessError) -> Self {
        let kind = match &error {
            VcsProcessError::Spawn { .. } => GitLabCliErrorKind::Unavailable,
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::Authentication),
                ..
            } => GitLabCliErrorKind::Authentication,
            VcsProcessError::Exit {
                failure_kind: Some(VcsProcessExitFailureKind::RateLimited),
                ..
            } => GitLabCliErrorKind::RateLimit,
            _ => GitLabCliErrorKind::Command,
        };
        Self::new(kind, cwd, Cause::new(error))
    }

    /// `GitLabMergeRequestNotFoundError.fromVcsError`.
    pub fn from_merge_request_vcs_error(cwd: &str, reference: &str, error: VcsProcessError) -> Self {
        if let VcsProcessError::Exit {
            failure_kind: Some(VcsProcessExitFailureKind::NotFound),
            ..
        } = &error
        {
            return Self::new(
                GitLabCliErrorKind::MergeRequestNotFound {
                    reference: reference.to_owned(),
                },
                cwd,
                Cause::new(error),
            );
        }
        Self::from_vcs_error(cwd, error)
    }
}

impl std::fmt::Display for GitLabCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitLabCliError {}

impl Serialize for GitLabCliError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut fields = json!({"operation": self.operation(), "command": "glab", "cwd": self.cwd});
        match &self.kind {
            GitLabCliErrorKind::MergeRequestNotFound { reference } | GitLabCliErrorKind::MergeRequestDecode { reference } => {
                fields["reference"] = json!(reference);
            }
            GitLabCliErrorKind::RepositoryDecode {
                repository: Some(repository), ..
            } => fields["repository"] = json!(repository),
            GitLabCliErrorKind::NamespaceDecode { namespace_path } => fields["namespacePath"] = json!(namespace_path),
            _ => {}
        }
        tagged(self.tag(), fields, Some(&self.cause)).serialize(serializer)
    }
}

impl CauseError for GitLabCliError {
    fn defect(&self) -> Value {
        error_defect(self.tag(), self.message(), Some(self.cause.defect()))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// `execute` input.
#[derive(Debug, Clone, Default)]
pub struct GitLabExecuteInput {
    pub cwd: String,
    pub args: Vec<String>,
    pub timeout_ms: Option<u64>,
    pub stdin: Option<String>,
    pub max_output_bytes: Option<usize>,
}

impl GitLabExecuteInput {
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

fn state_args(state: ChangeRequestStateFilter) -> &'static [&'static str] {
    match state {
        ChangeRequestStateFilter::Open => &[],
        ChangeRequestStateFilter::Closed => &["--closed"],
        ChangeRequestStateFilter::Merged => &["--merged"],
        ChangeRequestStateFilter::All => &["--all"],
    }
}

/// `normalizeHeadSelector`: `owner:branch` → `branch`.
fn normalize_head_selector(head_selector: &str) -> String {
    let trimmed = js_trim(head_selector);
    if let Some(index) = trimmed.find(':') {
        if index > 0 {
            let branch = js_trim(&trimmed[index + 1..]);
            if !branch.is_empty() {
                return branch.to_owned();
            }
        }
    }
    trimmed.to_owned()
}

fn source_ref_name(head_selector: &str, source: Option<&SourceControlRefSelector>) -> String {
    source.map_or_else(|| normalize_head_selector(head_selector), |source| source.ref_name.clone())
}

/// `parseRepositoryPath`: `group/sub/project` → (`group/sub`, `project`).
fn parse_repository_path(repository: &str) -> (Option<String>, String) {
    let parts: Vec<&str> = repository.split('/').map(js_trim).filter(|p| !p.is_empty()).collect();
    let project = parts.last().map_or_else(|| js_trim(repository).to_owned(), |p| (*p).to_owned());
    let namespace = (parts.len() > 1).then(|| parts[..parts.len() - 1].join("/"));
    (namespace, project)
}

/// `RawGitLabRepositoryCloneUrlsSchema` → clone URLs.
pub fn decode_gitlab_repository_clone_urls(raw: &str) -> Result<SourceControlRepositoryCloneUrls, String> {
    let value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    let field = |key: &str| value.get(key).and_then(trimmed_non_empty).ok_or_else(|| format!("Missing or invalid {key}"));
    let name_with_owner = field("path_with_namespace")?;
    let url = field("web_url")?;
    field("http_url_to_repo")?;
    let ssh_url = field("ssh_url_to_repo")?;
    Ok(SourceControlRepositoryCloneUrls { name_with_owner, url, ssh_url })
}

/// The `GitLabCli` service.
#[derive(Clone)]
pub struct GitLabCli {
    process: VcsProcess,
}

impl GitLabCli {
    pub fn new(process: VcsProcess) -> Self {
        Self { process }
    }

    async fn run(&self, input: &GitLabExecuteInput) -> Result<VcsProcessOutput, VcsProcessError> {
        let mut run = VcsProcessInput::new("GitLabCli.execute", "glab", input.args.iter().cloned(), input.cwd.as_str());
        run.timeout_ms = Some(input.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        run.stdin = input.stdin.clone();
        run.max_output_bytes = input.max_output_bytes;
        self.process.run(run).await
    }

    /// `execute`.
    pub async fn execute(&self, input: GitLabExecuteInput) -> Result<VcsProcessOutput, GitLabCliError> {
        self.run(&input).await.map_err(|error| GitLabCliError::from_vcs_error(&input.cwd, error))
    }

    async fn execute_merge_request(&self, cwd: &str, reference: &str, args: Vec<String>) -> Result<VcsProcessOutput, GitLabCliError> {
        self.run(&GitLabExecuteInput::new(cwd, args))
            .await
            .map_err(|error| GitLabCliError::from_merge_request_vcs_error(cwd, reference, error))
    }

    /// `listMergeRequests`.
    pub async fn list_merge_requests(
        &self,
        cwd: &str,
        head_selector: &str,
        source: Option<&SourceControlRefSelector>,
        state: ChangeRequestStateFilter,
        limit: Option<u32>,
    ) -> Result<Vec<NormalizedChangeRequest>, GitLabCliError> {
        let mut args = vec!["mr".to_owned(), "list".into(), "--source-branch".into(), source_ref_name(head_selector, source)];
        args.extend(state_args(state).iter().map(|a| (*a).to_owned()));
        args.extend(["--per-page".to_owned(), limit.unwrap_or(20).to_string(), "--output".into(), "json".into()]);
        let output = self.execute(GitLabExecuteInput::new(cwd, args)).await?;
        let raw = js_trim(&output.stdout);
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        decode_gitlab_merge_request_list_json(raw)
            .map_err(|e| GitLabCliError::new(GitLabCliErrorKind::MergeRequestListDecode, cwd, Cause::new(SchemaDecodeError(e))))
    }

    /// `getMergeRequest`.
    pub async fn get_merge_request(&self, cwd: &str, reference: &str) -> Result<NormalizedChangeRequest, GitLabCliError> {
        let output = self
            .execute_merge_request(cwd, reference, ["mr", "view", reference, "--output", "json"].map(String::from).to_vec())
            .await?;
        decode_gitlab_merge_request_json(js_trim(&output.stdout)).map_err(|e| {
            GitLabCliError::new(
                GitLabCliErrorKind::MergeRequestDecode {
                    reference: reference.to_owned(),
                },
                cwd,
                Cause::new(SchemaDecodeError(e)),
            )
        })
    }

    /// `getRepositoryCloneUrls`.
    pub async fn get_repository_clone_urls(&self, cwd: &str, repository: &str) -> Result<SourceControlRepositoryCloneUrls, GitLabCliError> {
        let output = self
            .execute(GitLabExecuteInput::new(
                cwd,
                ["api".to_owned(), format!("projects/{}", encode_uri_component(repository))],
            ))
            .await?;
        decode_gitlab_repository_clone_urls(js_trim(&output.stdout)).map_err(|e| {
            GitLabCliError::new(
                GitLabCliErrorKind::RepositoryDecode {
                    operation: "getRepositoryCloneUrls",
                    repository: Some(repository.to_owned()),
                },
                cwd,
                Cause::new(SchemaDecodeError(e)),
            )
        })
    }

    /// `createRepository`: resolves the namespace id when the path has one, then `POST projects`.
    pub async fn create_repository(
        &self,
        cwd: &str,
        repository: &str,
        visibility: SourceControlRepositoryVisibility,
    ) -> Result<SourceControlRepositoryCloneUrls, GitLabCliError> {
        let (namespace_path, project_path) = parse_repository_path(repository);
        let namespace_id = match &namespace_path {
            None => None,
            Some(namespace_path) => {
                let output = self
                    .execute(GitLabExecuteInput::new(
                        cwd,
                        ["api".to_owned(), format!("namespaces/{}", encode_uri_component(namespace_path))],
                    ))
                    .await?;
                let decoded: Result<f64, String> = serde_json::from_str::<Value>(js_trim(&output.stdout))
                    .map_err(|e| e.to_string())
                    .and_then(|value| value.get("id").and_then(Value::as_f64).ok_or_else(|| "Missing id".into()));
                Some(decoded.map_err(|e| {
                    GitLabCliError::new(
                        GitLabCliErrorKind::NamespaceDecode {
                            namespace_path: namespace_path.clone(),
                        },
                        cwd,
                        Cause::new(SchemaDecodeError(e)),
                    )
                })?)
            }
        };
        let mut args = vec![
            "api".to_owned(),
            "--method".into(),
            "POST".into(),
            "projects".into(),
            "--raw-field".into(),
            format!("path={project_path}"),
            "--raw-field".into(),
            format!("name={project_path}"),
            "--raw-field".into(),
            format!("visibility={}", visibility.as_str()),
        ];
        if let Some(id) = namespace_id {
            args.push("--raw-field".into());
            args.push(format!("namespace_id={}", js_number(id)));
        }
        let output = self.execute(GitLabExecuteInput::new(cwd, args)).await?;
        decode_gitlab_repository_clone_urls(js_trim(&output.stdout)).map_err(|e| {
            GitLabCliError::new(
                GitLabCliErrorKind::RepositoryDecode {
                    operation: "createRepository",
                    repository: Some(repository.to_owned()),
                },
                cwd,
                Cause::new(SchemaDecodeError(e)),
            )
        })
    }

    /// `createMergeRequest`: through the API, the body read by `glab` from the file
    /// (`description=@file`), never placed in argv.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_merge_request(
        &self,
        cwd: &str,
        base_branch: &str,
        head_selector: &str,
        source: Option<&SourceControlRefSelector>,
        target: Option<&SourceControlRefSelector>,
        title: &str,
        body_file: &str,
    ) -> Result<(), GitLabCliError> {
        let source_project = source.and_then(|s| s.repository.clone().or_else(|| s.owner.clone()));
        let mut args = vec![
            "api".to_owned(),
            "--method".into(),
            "POST".into(),
            "projects/:fullpath/merge_requests".into(),
            "--raw-field".into(),
            format!("source_branch={}", source_ref_name(head_selector, source)),
            "--raw-field".into(),
            format!("target_branch={}", target.map_or(base_branch, |t| t.ref_name.as_str())),
        ];
        if let Some(project) = source_project {
            args.push("--raw-field".into());
            args.push(format!("source_project_id={project}"));
        }
        args.extend([
            "--raw-field".to_owned(),
            format!("title={title}"),
            "--field".into(),
            format!("description=@{body_file}"),
        ]);
        self.execute(GitLabExecuteInput::new(cwd, args)).await.map(drop)
    }

    /// `getDefaultBranch`.
    pub async fn get_default_branch(&self, cwd: &str) -> Result<Option<String>, GitLabCliError> {
        let output = self.execute(GitLabExecuteInput::new(cwd, ["api", "projects/:fullpath"])).await?;
        let decode_error = |e: String| {
            GitLabCliError::new(
                GitLabCliErrorKind::RepositoryDecode {
                    operation: "getDefaultBranch",
                    repository: None,
                },
                cwd,
                Cause::new(SchemaDecodeError(e)),
            )
        };
        let value: Value = serde_json::from_str(js_trim(&output.stdout)).map_err(|e| decode_error(e.to_string()))?;
        if !value.is_object() {
            return Err(decode_error("Expected an object".into()));
        }
        match value.get("default_branch") {
            None | Some(Value::Null) => Ok(None),
            Some(branch) => trimmed_non_empty(branch).map(Some).ok_or_else(|| decode_error("Invalid default_branch".into())),
        }
    }

    /// `checkoutMergeRequest` (glab has no force flag).
    pub async fn checkout_merge_request(&self, cwd: &str, reference: &str) -> Result<(), GitLabCliError> {
        self.execute_merge_request(cwd, reference, ["mr", "checkout", reference].map(String::from).to_vec())
            .await
            .map(drop)
    }
}

/// `String(number)` for an integral JSON number.
fn js_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e21 {
        format!("{}", value as i64)
    } else {
        value.to_string()
    }
}
