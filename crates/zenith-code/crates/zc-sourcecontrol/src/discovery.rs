//! `SourceControlProviderDiscovery.ts`: how each provider is discovered (a CLI and its auth
//! command, an API probe, or a managed CLI with its own logic), the shared auth-output helpers,
//! and the refinement of remotes whose URL does not name a forge.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::future::BoxFuture;
use regex::Regex;
use zc_contracts::{
    EOption, SourceControlDiscoveryStatus, SourceControlProviderAuth, SourceControlProviderAuthStatus, SourceControlProviderDiscoveryItem,
    SourceControlProviderInfo, SourceControlProviderKind,
};
use zc_core::vcs_process::{VcsProcess, VcsProcessInput, VcsProcessOutput};

use crate::provider::SourceControlProviderContext;
use crate::util::{js_trim, split_lines, strip_vt_control_characters};

/// `SourceControlAuthProbeInput`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthProbeInput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

impl From<&VcsProcessOutput> for AuthProbeInput {
    fn from(output: &VcsProcessOutput) -> Self {
        Self {
            stdout: output.stdout.clone(),
            stderr: output.stderr.clone(),
            exit_code: output.exit_code,
        }
    }
}

/// `SourceControlUnknownRemoteRefinementInput`.
#[derive(Debug, Clone)]
pub struct RefinementInput {
    pub cwd: String,
    pub context: SourceControlProviderContext,
    pub auth: AuthProbeInput,
}

pub type ParseAuthFn = Arc<dyn Fn(&AuthProbeInput) -> SourceControlProviderAuth + Send + Sync>;
pub type RefineUnknownRemoteFn = Arc<dyn Fn(&RefinementInput) -> Option<SourceControlProviderInfo> + Send + Sync>;

/// `SourceControlCliDiscoverySpec`.
#[derive(Clone)]
pub struct CliDiscoverySpec {
    pub kind: SourceControlProviderKind,
    pub label: String,
    pub install_hint: String,
    pub executable: String,
    pub version_args: Vec<String>,
    pub auth_args: Vec<String>,
    pub remote_refinement_args: Option<Vec<String>>,
    pub probe_timeout_ms: Option<u64>,
    pub parse_auth: ParseAuthFn,
    pub refine_unknown_remote: Option<RefineUnknownRemoteFn>,
}

impl std::fmt::Debug for CliDiscoverySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliDiscoverySpec")
            .field("kind", &self.kind)
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}

/// `SourceControlApiDiscoverySpec`.
#[derive(Clone)]
pub struct ApiDiscoverySpec {
    pub kind: SourceControlProviderKind,
    pub label: String,
    pub install_hint: String,
    pub probe_auth: Arc<dyn Fn() -> BoxFuture<'static, SourceControlProviderAuth> + Send + Sync>,
}

/// `SourceControlManagedCliDiscoverySpec`.
#[async_trait]
pub trait ManagedCliDiscovery: Send + Sync {
    fn kind(&self) -> SourceControlProviderKind;
    fn label(&self) -> &str;
    fn install_hint(&self) -> &str;
    async fn probe(&self, cwd: &str) -> SourceControlProviderDiscoveryItem;
    async fn refine_unknown_remote(&self, cwd: &str, context: &SourceControlProviderContext) -> Option<SourceControlProviderInfo>;
}

/// `SourceControlProviderDiscoverySpec`.
#[derive(Clone)]
pub enum DiscoverySpec {
    Cli(CliDiscoverySpec),
    Api(ApiDiscoverySpec),
    ManagedCli(Arc<dyn ManagedCliDiscovery>),
}

impl DiscoverySpec {
    pub fn kind(&self) -> SourceControlProviderKind {
        match self {
            Self::Cli(spec) => spec.kind,
            Self::Api(spec) => spec.kind,
            Self::ManagedCli(spec) => spec.kind(),
        }
    }
}

const DEFAULT_PROBE_TIMEOUT_MS: u64 = 5_000;

fn probe_timeout_ms(spec: &CliDiscoverySpec) -> u64 {
    spec.probe_timeout_ms.unwrap_or(DEFAULT_PROBE_TIMEOUT_MS)
}

/// `firstNonEmptyLine`.
pub fn first_non_empty_line(text: &str) -> Option<String> {
    let stripped = strip_vt_control_characters(text);
    let line = split_lines(&stripped).map(js_trim).find(|line| !line.is_empty()).map(str::to_owned);
    line
}

/// `detailFromCause`: the trimmed message of an error, if any.
pub fn detail_from_cause(cause: &dyn std::fmt::Display) -> Option<String> {
    let message = cause.to_string();
    let trimmed = js_trim(&message);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn non_empty(value: Option<&str>) -> EOption<String> {
    match value.map(js_trim) {
        Some(trimmed) if !trimmed.is_empty() => EOption::some(trimmed.to_owned()),
        _ => EOption::none(),
    }
}

/// `providerAuth({status, account, host, detail})`.
pub fn provider_auth(status: SourceControlProviderAuthStatus, account: Option<&str>, host: Option<&str>, detail: Option<&str>) -> SourceControlProviderAuth {
    SourceControlProviderAuth {
        status,
        account: non_empty(account),
        host: non_empty(host),
        detail: non_empty(detail),
    }
}

fn unknown_auth(detail: Option<&str>) -> SourceControlProviderAuth {
    provider_auth(SourceControlProviderAuthStatus::Unknown, None, None, detail)
}

/// `combinedAuthOutput`.
pub fn combined_auth_output(input: &AuthProbeInput) -> String {
    [&input.stdout, &input.stderr]
        .into_iter()
        .filter(|entry| !js_trim(entry).is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitized_auth_lines(text: &str) -> Vec<String> {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    let token = TOKEN.get_or_init(|| Regex::new(r"(?i)^[-\s]*token(?:\s+scopes?)?:").expect("valid regex"));
    split_lines(text)
        .map(js_trim)
        .filter(|line| !line.is_empty() && !token.is_match(line))
        .map(str::to_owned)
        .collect()
}

/// `firstSafeAuthLine`: the first line that is not a token line.
pub fn first_safe_auth_line(text: &str) -> Option<String> {
    sanitized_auth_lines(text).into_iter().next()
}

/// `parseCliHost`.
pub fn parse_cli_host(text: &str) -> Option<String> {
    static LEADING: OnceLock<Regex> = OnceLock::new();
    static HOST: OnceLock<Regex> = OnceLock::new();
    let leading = LEADING.get_or_init(|| Regex::new(r"(?i)^[^a-z0-9]+").expect("valid regex"));
    let host = HOST.get_or_init(|| Regex::new(r"(?i)^[a-z0-9][a-z0-9.-]*(?::[0-9]+)?$").expect("valid regex"));
    sanitized_auth_lines(text)
        .into_iter()
        .map(|line| leading.replace(&line, "").into_owned())
        .find(|line| host.is_match(line))
}

/// `matchFirst`: the first non-empty first capture of the patterns, in order.
pub fn match_first(text: &str, patterns: &[&Regex]) -> Option<String> {
    patterns.iter().find_map(|pattern| {
        let value = pattern.captures(text)?.get(1).map(|m| js_trim(m.as_str()).to_owned())?;
        (!value.is_empty()).then_some(value)
    })
}

fn discovery_item(
    spec: &CliDiscoverySpec,
    status: SourceControlDiscoveryStatus,
    version: Option<String>,
    detail: Option<String>,
    auth: SourceControlProviderAuth,
) -> SourceControlProviderDiscoveryItem {
    SourceControlProviderDiscoveryItem {
        kind: spec.kind,
        label: spec.label.clone(),
        executable: Some(spec.executable.clone()),
        status,
        version: EOption(version),
        install_hint: spec.install_hint.clone(),
        detail: EOption(detail),
        auth,
    }
}

fn probe_input(operation: &str, command: &str, args: &[String], cwd: &str, timeout_ms: u64) -> VcsProcessInput {
    let mut input = VcsProcessInput::new(operation, command, args.iter().cloned(), cwd);
    input.timeout_ms = Some(timeout_ms);
    input.max_output_bytes = Some(8_000);
    input.append_truncation_marker = true;
    input
}

/// `probeSourceControlProvider`.
pub async fn probe_source_control_provider(spec: &DiscoverySpec, process: &VcsProcess, cwd: &str) -> SourceControlProviderDiscoveryItem {
    let spec = match spec {
        DiscoverySpec::ManagedCli(managed) => return managed.probe(cwd).await,
        DiscoverySpec::Api(api) => {
            let auth = (api.probe_auth)().await;
            return SourceControlProviderDiscoveryItem {
                kind: api.kind,
                label: api.label.clone(),
                executable: None,
                status: SourceControlDiscoveryStatus::Available,
                version: EOption::none(),
                install_hint: api.install_hint.clone(),
                detail: EOption::none(),
                auth,
            };
        }
        DiscoverySpec::Cli(spec) => spec,
    };
    probe_cli_spec(spec, process, cwd).await
}

/// The `type: "cli"` branch of `probeSourceControlProvider`.
pub async fn probe_cli_spec(spec: &CliDiscoverySpec, process: &VcsProcess, cwd: &str) -> SourceControlProviderDiscoveryItem {
    let timeout = probe_timeout_ms(spec);
    let version = process
        .run(probe_input(
            "source-control.discovery.probe",
            &spec.executable,
            &spec.version_args,
            cwd,
            timeout,
        ))
        .await;
    let version = match version {
        Ok(output) => first_non_empty_line(&output.stdout).or_else(|| first_non_empty_line(&output.stderr)),
        Err(error) => {
            return discovery_item(
                spec,
                SourceControlDiscoveryStatus::Missing,
                None,
                detail_from_cause(&error),
                unknown_auth(Some("Hosting integration command was not found on the server PATH.")),
            )
        }
    };
    let mut auth_input = probe_input("source-control.discovery.auth", &spec.executable, &spec.auth_args, cwd, timeout);
    auth_input.allow_non_zero_exit = true;
    let auth = match process.run(auth_input).await {
        Ok(output) => (spec.parse_auth)(&AuthProbeInput::from(&output)),
        Err(error) => unknown_auth(detail_from_cause(&error).as_deref()),
    };
    discovery_item(spec, SourceControlDiscoveryStatus::Available, version, None, auth)
}

/// `refineUnknownRemoteProvider`: asks every provider (in registration order) whether it owns
/// the remote of a context whose URL named no forge; the first answer wins.
pub async fn refine_unknown_remote_provider(
    specs: &[DiscoverySpec],
    process: &VcsProcess,
    cwd: &str,
    context: Option<SourceControlProviderContext>,
) -> Option<SourceControlProviderContext> {
    let context = context?;
    if context.provider.kind != SourceControlProviderKind::Unknown {
        return Some(context);
    }
    let mut found = None;
    for spec in specs {
        let provider = match spec {
            DiscoverySpec::ManagedCli(managed) => managed.refine_unknown_remote(cwd, &context).await,
            DiscoverySpec::Cli(spec) => match &spec.refine_unknown_remote {
                None => None,
                Some(refine) => {
                    let args = spec.remote_refinement_args.as_ref().unwrap_or(&spec.auth_args);
                    let mut input = probe_input(
                        "source-control.discovery.refine-unknown-remote",
                        &spec.executable,
                        args,
                        cwd,
                        probe_timeout_ms(spec),
                    );
                    input.allow_non_zero_exit = true;
                    match process.run(input).await {
                        Ok(output) => refine(&RefinementInput {
                            cwd: cwd.to_owned(),
                            context: context.clone(),
                            auth: AuthProbeInput::from(&output),
                        }),
                        Err(_) => None,
                    }
                }
            },
            DiscoverySpec::Api(_) => None,
        };
        if found.is_none() {
            found = provider;
        }
    }
    Some(match found {
        Some(provider) => SourceControlProviderContext { provider, ..context },
        None => context,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_control_characters_from_the_first_line() {
        assert_eq!(
            first_non_empty_line("\u{1b}[1mtea version 0.16.0\u{1b}[0m\n").as_deref(),
            Some("tea version 0.16.0")
        );
        assert_eq!(first_non_empty_line("\n  \r\n second\n"), Some("second".into()));
    }

    #[test]
    fn skips_token_lines() {
        assert_eq!(first_safe_auth_line("  - Token: gho_secret\nLogged in"), Some("Logged in".into()));
        assert_eq!(parse_cli_host("✓ gitlab.example.test:8443\n"), Some("gitlab.example.test:8443".into()));
    }
}
