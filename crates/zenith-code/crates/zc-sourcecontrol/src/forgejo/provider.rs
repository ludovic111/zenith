//! `ForgejoSourceControlProvider.ts`: the Forgejo/Gitea provider over [`ForgejoCli`] and its
//! discovery (`tea` as a plain CLI spec, and the managed `fj`-first probe).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_contracts::{
    ChangeRequest, SourceControlDiscoveryStatus, SourceControlProviderAuthStatus as Auth, SourceControlProviderDiscoveryItem, SourceControlProviderInfo,
    SourceControlProviderKind, SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility,
};
use zc_core::vcs_process::{VcsProcess, VcsProcessInput};

use crate::discovery::{probe_cli_spec, provider_auth, AuthProbeInput, CliDiscoverySpec, ManagedCliDiscovery, RefinementInput};
use crate::errors::{Cause, SourceControlProviderError};
use crate::forgejo::cli::*;
use crate::forgejo::pull_requests::{decode_forgejo_pull_request, to_forgejo_change_request, ForgejoPullRequest};
use crate::github::cli::SchemaDecodeError;
use crate::provider::*;
use crate::util::encode_uri_component;

const KIND: SourceControlProviderKind = SourceControlProviderKind::Forgejo;
const LABEL: &str = "Forgejo / Gitea";
const INSTALL_HINT: &str = "Install `fj` 0.6 or later from https://codeberg.org/forgejo-contrib/forgejo-cli and run `fj --host <server-url> auth add-token`, or install `tea` 0.16 or later from https://gitea.com/gitea/tea and run `tea login add` for each Forgejo or Gitea server.";

fn host_of(url: &str) -> Option<String> {
    parse_forgejo_remote(url).map(|remote| remote.host)
}

/// The `tea` discovery spec.
pub fn discovery() -> CliDiscoverySpec {
    CliDiscoverySpec {
        kind: KIND,
        label: LABEL.into(),
        install_hint: INSTALL_HINT.into(),
        executable: "tea".into(),
        version_args: vec!["--version".into()],
        auth_args: ["login", "status", "--output", "json"].map(String::from).to_vec(),
        remote_refinement_args: Some(["login", "list", "--output", "json"].map(String::from).to_vec()),
        probe_timeout_ms: None,
        parse_auth: Arc::new(|input: &AuthProbeInput| {
            let logins = parse_forgejo_logins(&input.stdout);
            match logins.iter().find(|l| l.default == "true").or_else(|| logins.first()) {
                Some(login) => provider_auth(
                    if login.valid.as_deref() == Some("true") {
                        Auth::Authenticated
                    } else {
                        Auth::Unauthenticated
                    },
                    Some(&login.user),
                    host_of(&login.url).as_deref(),
                    None,
                ),
                None => provider_auth(
                    Auth::Unauthenticated,
                    None,
                    None,
                    Some("Run `tea login add` to authenticate a Forgejo or Gitea server."),
                ),
            }
        }),
        refine_unknown_remote: Some(Arc::new(|input: &RefinementInput| {
            let remote = parse_forgejo_remote(&input.context.remote_url)?;
            let login = match_forgejo_login(
                &parse_forgejo_logins(&input.auth.stdout),
                &remote,
                input.context.requested_host.as_deref(),
                false,
            )?;
            Some(SourceControlProviderInfo {
                kind: KIND,
                name: LABEL.into(),
                base_url: login.url,
            })
        })),
    }
}

/// `makeDiscovery`: prefers a configured `fj` account, falls back to `tea`.
pub struct ForgejoDiscovery {
    cli: ForgejoCli,
    process: VcsProcess,
}

impl ForgejoDiscovery {
    pub fn new(cli: ForgejoCli, process: VcsProcess) -> Self {
        Self { cli, process }
    }
}

#[async_trait]
impl ManagedCliDiscovery for ForgejoDiscovery {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn label(&self) -> &str {
        LABEL
    }

    fn install_hint(&self) -> &str {
        INSTALL_HINT
    }

    async fn probe(&self, cwd: &str) -> SourceControlProviderDiscoveryItem {
        let mut remote_input = VcsProcessInput::new("source-control.discovery.remote", "git", ["remote", "get-url", "origin"], cwd);
        remote_input.allow_non_zero_exit = true;
        remote_input.timeout_ms = Some(5_000);
        remote_input.max_output_bytes = Some(8_000);
        let remote_url = self.process.run(remote_input).await.map(|o| o.stdout.trim().to_owned()).unwrap_or_default();
        let credentials = self.cli.list_logins(cwd, ForgejoCommand::Fj, Some(&remote_url)).await;
        let logins = credentials.clone().unwrap_or_default();
        let remote = parse_forgejo_remote(&remote_url);
        let login = remote
            .as_ref()
            .and_then(|remote| match_forgejo_login(&logins, remote, None, false))
            .or_else(|| logins.iter().find(|l| l.default == "true").cloned())
            .or_else(|| logins.first().cloned());
        let credentials_failed = credentials.is_err();
        let parse_login = login.clone();
        let mut fj_spec = discovery();
        fj_spec.executable = "fj".into();
        fj_spec.version_args = vec!["version".into()];
        fj_spec.auth_args = match &login {
            Some(login) => vec!["--host".into(), login.url.clone(), "whoami".into()],
            None => vec!["auth".into(), "list".into()],
        };
        fj_spec.parse_auth = Arc::new(move |result: &AuthProbeInput| {
            if credentials_failed {
                return provider_auth(
                    Auth::Unknown,
                    None,
                    None,
                    Some("Could not read fj authentication storage. Authenticate again with fj."),
                );
            }
            match &parse_login {
                Some(login) if result.exit_code == 0 => provider_auth(Auth::Authenticated, Some(&login.user), host_of(&login.url).as_deref(), None),
                _ => provider_auth(
                    Auth::Unauthenticated,
                    None,
                    None,
                    Some("Authenticate this server with `fj --host <server-url> auth add-token`."),
                ),
            }
        });
        let fj = probe_cli_spec(&fj_spec, &self.process, cwd).await;
        // A configured fj account owns its requests, including authentication errors.
        if fj.status == SourceControlDiscoveryStatus::Available && (login.is_some() || credentials_failed) {
            if let Some(login) = &login {
                if fj.auth.status == Auth::Authenticated {
                    let auth = match self.cli.get_account(cwd, &login.url).await {
                        Ok(account) => provider_auth(Auth::Authenticated, Some(&account), host_of(&login.url).as_deref(), None),
                        Err(error) => provider_auth(Auth::Unknown, None, host_of(&login.url).as_deref(), Some(&error.detail)),
                    };
                    return SourceControlProviderDiscoveryItem { auth, ..fj };
                }
            }
            return fj;
        }
        let tea = probe_cli_spec(&discovery(), &self.process, cwd).await;
        if tea.status == SourceControlDiscoveryStatus::Available || fj.status == SourceControlDiscoveryStatus::Missing {
            tea
        } else {
            fj
        }
    }

    async fn refine_unknown_remote(&self, cwd: &str, context: &SourceControlProviderContext) -> Option<SourceControlProviderInfo> {
        let remote = parse_forgejo_remote(&context.remote_url)?;
        for command in [ForgejoCommand::Fj, ForgejoCommand::Tea] {
            let logins = self.cli.list_logins(cwd, command, Some(&context.remote_url)).await.unwrap_or_default();
            if let Some(login) = match_forgejo_login(&logins, &remote, context.requested_host.as_deref(), false) {
                return Some(SourceControlProviderInfo {
                    kind: KIND,
                    name: LABEL.into(),
                    base_url: login.url,
                });
            }
        }
        None
    }
}

/// Any failure of a Forgejo operation (`mapError`): a CLI error keeps its command and detail.
enum Failure {
    Cli(ForgejoCliError),
    Other(Cause),
}

impl From<ForgejoCliError> for Failure {
    fn from(error: ForgejoCliError) -> Self {
        Self::Cli(error)
    }
}

fn map_failure(operation: &str, cwd: &str, failure: Failure) -> SourceControlProviderError {
    match failure {
        Failure::Cli(error) => SourceControlProviderError::new(KIND, operation, cwd, error.detail.clone())
            .with_command(error.command.as_str())
            .with_cause(Cause::new(error)),
        Failure::Other(cause) => SourceControlProviderError::new(KIND, operation, cwd, "Forgejo operation failed.").with_cause(cause),
    }
}

struct RepositoryJson {
    full_name: String,
    clone_url: String,
    ssh_url: String,
    default_branch: Option<String>,
}

fn decode_repository(value: &Value) -> Option<RepositoryJson> {
    let map = value.as_object()?;
    Some(RepositoryJson {
        full_name: map.get("full_name")?.as_str()?.to_owned(),
        clone_url: map.get("clone_url")?.as_str()?.to_owned(),
        ssh_url: map.get("ssh_url")?.as_str()?.to_owned(),
        default_branch: match map.get("default_branch") {
            None | Some(Value::Null) => None,
            Some(Value::String(branch)) => Some(branch.clone()),
            Some(_) => return None,
        },
    })
}

fn clone_urls(raw: &RepositoryJson) -> SourceControlRepositoryCloneUrls {
    SourceControlRepositoryCloneUrls {
        name_with_owner: raw.full_name.clone(),
        url: raw.clone_url.clone(),
        ssh_url: raw.ssh_url.clone(),
    }
}

/// `ForgejoSourceControlProvider`.
#[derive(Clone)]
pub struct ForgejoSourceControlProvider {
    cli: ForgejoCli,
    process: VcsProcess,
}

fn target(cwd: &str, context: Option<&SourceControlProviderContext>) -> ForgejoRepositoryInput {
    ForgejoRepositoryInput {
        cwd: cwd.to_owned(),
        context: context.cloned(),
        ..ForgejoRepositoryInput::default()
    }
}

impl ForgejoSourceControlProvider {
    pub fn new(cli: ForgejoCli, process: VcsProcess) -> Self {
        Self { cli, process }
    }

    pub fn cli(&self) -> &ForgejoCli {
        &self.cli
    }

    /// `request(input, schema)`: an API call decoded with `decode`.
    async fn request<T>(&self, input: ForgejoApiInput, decode: impl Fn(&Value) -> Option<T>) -> Result<T, ForgejoCliError> {
        let cwd = input.target.cwd.clone();
        let output = self.cli.api(&input).await?;
        serde_json::from_str::<Value>(&output.stdout)
            .ok()
            .and_then(|value| decode(&value))
            .ok_or_else(|| {
                ForgejoCliError::new(ForgejoCommand::Tea, &cwd, "Forgejo API returned an invalid response.")
                    .with_reason(ForgejoErrorReason::InvalidResponse)
                    .with_cause(Cause::new(SchemaDecodeError("The response does not match the schema".into())))
            })
    }

    async fn get_pull(&self, input: &GetChangeRequestInput) -> Result<ForgejoPullRequest, ForgejoCliError> {
        let mut repository_input = target(&input.cwd, input.context.as_ref());
        repository_input.reference = Some(input.reference.clone());
        let repo = self.cli.resolve_repository(&repository_input).await?;
        let number = pull_number(&input.reference)
            .ok_or_else(|| ForgejoCliError::new(ForgejoCommand::Tea, &input.cwd, "Specify a pull request number or Forgejo pull request URL."))?;
        self.request(
            ForgejoApiInput {
                target: repository_input,
                path: format!("{}/pulls/{number}", forgejo_repository_path(&repo.repository)),
                ..ForgejoApiInput::default()
            },
            decode_forgejo_pull_request,
        )
        .await
    }
}

/// The pull request number of `#12`, `12` or a `/pulls/12` URL.
fn pull_number(reference: &str) -> Option<String> {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN
        .get_or_init(|| regex::Regex::new(r"(?:^#?|/pulls/)([0-9]+)(?:/[^?#]*)?(?:[?#].*)?$").expect("valid regex"))
        .captures(reference)
        .and_then(|c| c.get(1).map(|m| m.as_str().to_owned()))
}

#[async_trait]
impl SourceControlProvider for ForgejoSourceControlProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        let result: Result<Vec<ChangeRequest>, Failure> = async {
            let repository_input = target(&input.cwd, input.context.as_ref());
            let repo = self.cli.resolve_repository(&repository_input).await?;
            let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
            let branch = source_branch(&input.head_selector, input.source.as_ref());
            let limit = input.limit.unwrap_or(20) as usize;
            let state = if input.state == ChangeRequestStateFilter::Merged {
                "closed"
            } else {
                input.state.as_str()
            };
            let mut results = Vec::new();
            let mut page = 1;
            while results.len() < limit {
                let items = self
                    .request(
                        ForgejoApiInput {
                            target: repository_input.clone(),
                            path: format!(
                                "{}/pulls?state={state}&sort=recentupdate&limit=50&page={page}",
                                forgejo_repository_path(&repo.repository)
                            ),
                            ..ForgejoApiInput::default()
                        },
                        |value| value.as_array()?.iter().map(decode_forgejo_pull_request).collect::<Option<Vec<_>>>(),
                    )
                    .await?;
                for item in &items {
                    if item.head.ref_name != branch
                        || source
                            .as_ref()
                            .and_then(|s| s.repository.as_ref())
                            .is_some_and(|r| item.head.repo.as_ref().map(|h| &h.full_name) != Some(r))
                        || source
                            .as_ref()
                            .and_then(|s| s.owner.as_ref())
                            .is_some_and(|o| item.head.repo.as_ref().map(|h| &h.owner_login) != Some(o))
                    {
                        continue;
                    }
                    let normalized = to_forgejo_change_request(item);
                    if input.state == ChangeRequestStateFilter::All || normalized.state.as_str() == input.state.as_str() {
                        results.push(normalized);
                    }
                }
                if items.is_empty() {
                    break;
                }
                page += 1;
            }
            results.truncate(limit);
            Ok(results)
        }
        .await;
        result.map_err(|failure| map_failure("listChangeRequests", &input.cwd, failure))
    }

    async fn get_change_request(&self, input: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        self.get_pull(&input)
            .await
            .map(|pull| to_forgejo_change_request(&pull))
            .map_err(|error| map_failure("getChangeRequest", &input.cwd, error.into()))
    }

    async fn create_change_request(&self, input: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        let result: Result<(), Failure> = async {
            let repository_input = target(&input.cwd, input.context.as_ref());
            let repo = self.cli.resolve_repository(&repository_input).await?;
            let source = source_control_ref_from_input(&input.head_selector, input.source.as_ref());
            let owner = source.as_ref().and_then(|s| {
                s.owner
                    .clone()
                    .or_else(|| s.repository.as_ref().and_then(|r| r.split('/').next().map(str::to_owned)))
            });
            let head = source_branch(&input.head_selector, input.source.as_ref());
            let body = tokio::fs::read_to_string(&input.body_file)
                .await
                .map_err(|error| Failure::Other(Cause::new(error)))?;
            let target_repository = input.target.as_ref().and_then(|t| t.repository.clone()).unwrap_or(repo.repository.clone());
            self.cli
                .api(&ForgejoApiInput {
                    target: repository_input,
                    path: format!("{}/pulls", forgejo_repository_path(&target_repository)),
                    method: Some("POST".into()),
                    body: Some(json!({
                        "base": input.target.as_ref().map_or(input.base_ref_name.as_str(), |t| t.ref_name.as_str()),
                        "head": match owner { Some(owner) => format!("{owner}:{head}"), None => head },
                        "title": input.title,
                        "body": body,
                    })),
                })
                .await?;
            Ok(())
        }
        .await;
        result.map_err(|failure| map_failure("createChangeRequest", &input.cwd, failure))
    }

    async fn get_repository_clone_urls(&self, input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        let result: Result<SourceControlRepositoryCloneUrls, ForgejoCliError> = async {
            let mut repository_input = target(&input.cwd, input.context.as_ref());
            repository_input.repository = Some(input.repository.clone());
            let repo = self.cli.resolve_repository(&repository_input).await?;
            let raw = self
                .request(
                    ForgejoApiInput {
                        target: repository_input,
                        path: forgejo_repository_path(&repo.repository),
                        ..ForgejoApiInput::default()
                    },
                    decode_repository,
                )
                .await?;
            Ok(clone_urls(&raw))
        }
        .await;
        result.map_err(|error| map_failure("getRepositoryCloneUrls", &input.cwd, error.into()))
    }

    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        let result: Result<SourceControlRepositoryCloneUrls, ForgejoCliError> = async {
            let repository_input = ForgejoRepositoryInput {
                cwd: input.cwd.clone(),
                repository: Some(input.repository.clone()),
                ..ForgejoRepositoryInput::default()
            };
            let repo = self.cli.resolve_repository(&repository_input).await?;
            let user = self
                .request(
                    ForgejoApiInput {
                        target: repository_input.clone(),
                        path: "user".into(),
                        ..ForgejoApiInput::default()
                    },
                    |value| value.get("login")?.as_str().map(str::to_owned),
                )
                .await?;
            let mut parts = repo.repository.split('/');
            let owner = parts.next().unwrap_or_default().to_owned();
            let name = parts.next().map(str::to_owned);
            let raw = self
                .request(
                    ForgejoApiInput {
                        target: repository_input,
                        path: if owner == user {
                            "user/repos".into()
                        } else {
                            format!("orgs/{}/repos", encode_uri_component(&owner))
                        },
                        method: Some("POST".into()),
                        body: Some(json!({
                            "name": name,
                            "private": input.visibility == SourceControlRepositoryVisibility::Private,
                            "auto_init": false,
                        })),
                    },
                    decode_repository,
                )
                .await?;
            Ok(clone_urls(&raw))
        }
        .await;
        result.map_err(|error| map_failure("createRepository", &input.cwd, error.into()))
    }

    async fn get_default_branch(&self, input: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        let result: Result<Option<String>, ForgejoCliError> = async {
            let repository_input = target(&input.cwd, input.context.as_ref());
            let repo = self.cli.resolve_repository(&repository_input).await?;
            let raw = self
                .request(
                    ForgejoApiInput {
                        target: repository_input,
                        path: forgejo_repository_path(&repo.repository),
                        ..ForgejoApiInput::default()
                    },
                    decode_repository,
                )
                .await?;
            Ok(raw.default_branch)
        }
        .await;
        result.map_err(|error| map_failure("getDefaultBranch", &input.cwd, error.into()))
    }

    async fn checkout_change_request(&self, input: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        let result: Result<(), Failure> = async {
            let mut repository_input = target(&input.cwd, input.context.as_ref());
            repository_input.reference = Some(input.reference.clone());
            let repo = self.cli.resolve_repository(&repository_input).await?;
            let pull = self
                .get_pull(&GetChangeRequestInput {
                    cwd: input.cwd.clone(),
                    context: input.context.clone(),
                    reference: input.reference.clone(),
                })
                .await?;
            let git = |args: Vec<String>, allow_non_zero_exit: bool| {
                let mut run = VcsProcessInput::new("ForgejoSourceControlProvider.checkoutChangeRequest", "git", args, input.cwd.as_str());
                run.allow_non_zero_exit = allow_non_zero_exit;
                run
            };
            let other = |error: zc_core::vcs_process::VcsProcessError| Failure::Other(Cause::new(error));
            if repo.command == ForgejoCommand::Fj {
                // fj checkout cannot target a repository outside the local remotes.
                let urls = self
                    .request(
                        ForgejoApiInput {
                            target: repository_input,
                            path: forgejo_repository_path(&repo.repository),
                            ..ForgejoApiInput::default()
                        },
                        decode_repository,
                    )
                    .await?;
                let use_ssh = input.context.as_ref().and_then(|c| parse_forgejo_remote(&c.remote_url)).is_some_and(|r| r.ssh);
                self.process
                    .run(git(
                        vec![
                            "fetch".into(),
                            "--".into(),
                            if use_ssh { urls.ssh_url.clone() } else { urls.clone_url.clone() },
                            format!("refs/pull/{}/head", pull.number),
                        ],
                        false,
                    ))
                    .await
                    .map_err(other)?;
                let branch = format!("pulls/{}", pull.number);
                let existing = self
                    .process
                    .run(git(
                        vec!["show-ref".into(), "--verify".into(), "--quiet".into(), format!("refs/heads/{branch}")],
                        true,
                    ))
                    .await
                    .map_err(other)?;
                let args = if existing.exit_code == 0 {
                    vec!["checkout".into(), branch]
                } else {
                    vec!["checkout".into(), "-b".into(), branch, "FETCH_HEAD".into()]
                };
                self.process.run(git(args, false)).await.map_err(other)?;
            } else {
                self.cli
                    .execute(ForgejoExecuteInput::new(
                        ForgejoCommand::Tea,
                        &input.cwd,
                        [
                            "pulls".to_owned(),
                            "checkout".into(),
                            "--login".into(),
                            repo.login.clone(),
                            "--repo".into(),
                            repo.repository.clone(),
                            "--branch".into(),
                            pull.number.to_string(),
                        ],
                    ))
                    .await?;
            }
            if input.force {
                // tea leaves an existing PR branch at its old tip. Keep dirty files safe while
                // bringing the selected branch to the PR revision we fetched.
                self.process
                    .run(git(vec!["reset".into(), "--keep".into(), pull.head.sha.clone()], false))
                    .await
                    .map_err(other)?;
            }
            Ok(())
        }
        .await;
        result.map_err(|failure| map_failure("checkoutChangeRequest", &input.cwd, failure))
    }
}
