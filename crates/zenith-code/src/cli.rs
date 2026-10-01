//! The command line (plan §6.17): the contract the dashboards depend on.
//!
//! ```text
//! zenith-code serve [cwd] [--host H] [--port N] [--base-dir D] [--mode web|desktop] [--dev-url U] …
//! zenith-code auth pairing create [--ttl 2m] [--admin] [--label L] [--base-url U] [--json]
//! zenith-code auth pairing list [--json]
//! zenith-code auth pairing revoke <id>
//! zenith-code auth session issue [--ttl 12h] [--label L] [--subject S] [--token-only] [--json]
//! zenith-code auth session list [--json]
//! zenith-code auth session revoke <session-id>
//! zenith-code project add <path> [--title T]
//! zenith-code project remove <project> [--force]
//! zenith-code project rename <project> <title>
//! zenith-code dev-serve [--host H] [--port N] [--static-dir D] [--conformance]
//! ```
//!
//! Every `auth` and `project` command also takes `--base-dir` and `--dev-url`. Flags may follow positionals
//! (the dashboards append `--base-dir X` last). Usage errors go to stderr with exit code 1, like
//! Effect CLI. Output formats are `zc_auth::cli_format`'s, printed with one extra newline the
//! way `Console.log` prints the TS formatter's text.

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use zc_auth::cli_format::{
    console_log, format_issued_pairing_credential, format_issued_session, format_pairing_credential_list, format_pairing_revoke, format_session_list,
    format_session_revoke,
};
use zc_auth::{
    system_clock, CookieNameInput, CreatePairingLinkInput, EnvironmentAuth, IssueBearerSessionInput, ServerMode, ADMINISTRATIVE_SCOPES,
    INTERNAL_ADMINISTRATIVE_BOOTSTRAP_SUBJECT, STANDARD_CLIENT_SCOPES,
};
use zc_core::config::{
    parse_duration_input, resolve_server_config, CliServerFlags, LogLevel, ProcessEnv, ResolveServerConfigOptions, RuntimeMode, ServerConfig,
    StartupPresentation,
};
use zc_core::ServerSecretStore;
use zc_db::Db;

#[derive(Debug, Parser)]
#[command(name = "zenith-code", version, about = "zenith code's server and control plane")]
pub struct Cli {
    /// Minimum log level (`All`, `Trace`, `Debug`, `Info`, `Warning`, `Error`, `Fatal`, `None`).
    #[arg(long, global = true)]
    pub log_level: Option<String>,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the server without opening a browser and print headless pairing details.
    Serve(ServeArgs),
    /// Manage the local auth control plane for headless deployments.
    Auth(AuthArgs),
    /// Manage projects.
    Project(ProjectArgs),
    /// Development server without real authentication (tests, RPC conformance).
    DevServe(DevServeArgs),
}

/// `serve`'s flags (`sharedServerCommandFlags`).
#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Working directory for provider sessions (defaults to the current directory).
    pub cwd: Option<String>,
    #[arg(long)]
    pub mode: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub base_dir: Option<String>,
    #[arg(long)]
    pub dev_url: Option<String>,
    #[arg(long)]
    pub no_browser: bool,
    #[arg(long)]
    pub bootstrap_fd: Option<i64>,
    #[arg(long)]
    pub auto_bootstrap_project_from_cwd: bool,
    #[arg(long = "log-websocket-events", alias = "log-ws-events")]
    pub log_websocket_events: bool,
    #[arg(long)]
    pub tailscale_serve: bool,
    #[arg(long)]
    pub tailscale_serve_port: Option<u16>,
    /// The built web client (else `ZENITH_CODE_STATIC_DIR`, or the usual locations).
    #[arg(long)]
    pub static_dir: Option<String>,
}

#[derive(Debug, Args)]
pub struct AuthArgs {
    #[command(subcommand)]
    pub command: AuthCommand,
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Manage one-time client pairing tokens.
    Pairing {
        #[command(subcommand)]
        command: PairingCommand,
    },
    /// Manage bearer sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
}

/// `authLocationFlags`.
#[derive(Debug, Args, Clone, Default)]
pub struct AuthLocation {
    /// Explicit data directory; runtime state is stored under userdata (like T3CODE_HOME).
    #[arg(long)]
    pub base_dir: Option<String>,
    /// Dev web URL (selects the dev state directory when no base dir is given).
    #[arg(long)]
    pub dev_url: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum PairingCommand {
    /// Issue a new client pairing token.
    Create {
        #[command(flatten)]
        location: AuthLocation,
        /// TTL, for example `5m`, `1h`, `30d`, or `15 minutes`.
        #[arg(long)]
        ttl: Option<String>,
        /// Optional human-readable label.
        #[arg(long)]
        label: Option<String>,
        /// Optional public base URL used to print a ready `/pair#token=...` link.
        #[arg(long)]
        base_url: Option<String>,
        /// Emit JSON instead of human-readable output.
        #[arg(long)]
        json: bool,
        /// Grant administrative scopes (owner client) instead of standard ones.
        #[arg(long)]
        admin: bool,
    },
    /// List active client pairing tokens without revealing their secrets.
    List {
        #[command(flatten)]
        location: AuthLocation,
        #[arg(long)]
        json: bool,
    },
    /// Revoke an active client pairing token.
    Revoke {
        #[command(flatten)]
        location: AuthLocation,
        /// Pairing credential id to revoke.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    /// Issue a scoped bearer access token for headless or remote clients.
    Issue {
        #[command(flatten)]
        location: AuthLocation,
        #[arg(long)]
        ttl: Option<String>,
        #[arg(long)]
        label: Option<String>,
        /// Optional session subject.
        #[arg(long)]
        subject: Option<String>,
        /// Print only the issued bearer token.
        #[arg(long)]
        token_only: bool,
        #[arg(long)]
        json: bool,
    },
    /// List active sessions without revealing bearer tokens.
    List {
        #[command(flatten)]
        location: AuthLocation,
        #[arg(long)]
        json: bool,
    },
    /// Revoke an active session.
    Revoke {
        #[command(flatten)]
        location: AuthLocation,
        /// Session id to revoke.
        session_id: String,
    },
}

#[derive(Debug, Args)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Add a project.
    Add {
        #[command(flatten)]
        location: AuthLocation,
        /// Workspace root to add as a project.
        path: String,
        /// Optional project title.
        #[arg(long)]
        title: Option<String>,
    },
    /// Remove a project.
    Remove {
        #[command(flatten)]
        location: AuthLocation,
        /// Project id or workspace root to remove.
        project: String,
        /// Delete the project and all of its threads.
        #[arg(long)]
        force: bool,
    },
    /// Rename a project.
    Rename {
        #[command(flatten)]
        location: AuthLocation,
        /// Project id or workspace root to rename.
        project: String,
        /// New project title.
        title: String,
    },
}

#[derive(Debug, Args)]
pub struct DevServeArgs {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = 0)]
    pub port: u16,
    #[arg(long)]
    pub static_dir: Option<PathBuf>,
    /// Serve the dummy RPC group of `code/scripts/rpc-conformance`.
    #[arg(long)]
    pub conformance: bool,
    /// Projects for the Sessions page, as `[{id, title, workspaceRoot}]` (the real server reads them from its database).
    #[arg(long)]
    pub projects: Option<PathBuf>,
}

impl Cli {
    /// `--json` / `--token-only` auth commands log errors only (`quietLogs`).
    pub fn quiet_logs(&self) -> bool {
        match &self.command {
            Command::Auth(AuthArgs {
                command: AuthCommand::Pairing { command },
            }) => matches!(command, PairingCommand::Create { json: true, .. } | PairingCommand::List { json: true, .. }),
            Command::Auth(AuthArgs {
                command: AuthCommand::Session { command },
            }) => matches!(
                command,
                SessionCommand::Issue { json: true, .. } | SessionCommand::Issue { token_only: true, .. } | SessionCommand::List { json: true, .. }
            ),
            _ => false,
        }
    }
}

/// Parses the command line; usage errors exit 1 (help and version exit 0).
pub fn parse() -> Result<Cli, ExitCode> {
    match Cli::try_parse() {
        Ok(cli) => Ok(cli),
        Err(error) => {
            let _ = error.print();
            Err(if error.use_stderr() { ExitCode::from(1) } else { ExitCode::SUCCESS })
        }
    }
}

/// `DurationFromString`, in milliseconds.
fn parse_ttl(ttl: Option<&str>) -> anyhow::Result<Option<i64>> {
    ttl.map(|value| {
        parse_duration_input(value)
            .and_then(|ms| i64::try_from(ms).ok())
            .with_context(|| format!("Invalid duration {value:?}. Use values like 5m, 1h, 30d, or 15 minutes."))
    })
    .transpose()
}

fn log_level(raw: Option<&str>) -> Option<LogLevel> {
    raw.and_then(LogLevel::parse)
}

/// `resolveCliAuthConfig`: the server config for a base dir (and dev URL).
pub async fn resolve_auth_config(location: &AuthLocation, log: Option<&str>) -> anyhow::Result<ServerConfig> {
    let flags = CliServerFlags {
        base_dir: location.base_dir.clone(),
        dev_url: location.dev_url.clone(),
        ..CliServerFlags::default()
    };
    resolve_server_config(&flags, log_level(log), ResolveServerConfigOptions::default(), &ProcessEnv)
        .await
        .context("could not resolve the server configuration")
}

/// The cookie-name input of a resolved config.
pub fn cookie_input(config: &ServerConfig, environment_id: String) -> CookieNameInput {
    CookieNameInput {
        mode: match config.mode {
            RuntimeMode::Desktop => ServerMode::Desktop,
            RuntimeMode::Web => ServerMode::Web,
        },
        port: config.port,
        host: config.host.clone(),
        instance_key: config.paths.state_dir.to_string_lossy().into_owned(),
        environment_id,
        development: config.dev_url.is_some(),
    }
}

/// Opens the database (migrating it), the secret store, the environment id and the auth
/// services, as `EnvironmentAuth.runtimeLayer` does for the CLI and the server.
pub async fn open_environment_auth(config: &ServerConfig) -> anyhow::Result<(Db, Arc<EnvironmentAuth>)> {
    let db_path = config.paths.db_path.clone();
    let db = tokio::task::spawn_blocking(move || Db::open(&db_path))
        .await?
        .context("could not open the database")?;
    let secrets = ServerSecretStore::open(&config.paths.secrets_dir)
        .await
        .context("could not open the secret store")?;
    let environment_id = zc_core::environment_id::read_or_create_environment_id(&config.paths.state_dir, &config.paths.environment_id_path)
        .await
        .context("could not read the environment id")?;
    let auth = EnvironmentAuth::open(db.clone(), secrets, cookie_input(config, environment_id), system_clock())
        .await
        .context("could not read the server signing key")?;
    Ok((db, Arc::new(auth)))
}

pub(crate) fn print_stdout(text: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(console_log(text).as_bytes())?;
    stdout.flush()?;
    Ok(())
}

/// Runs an `auth …` command.
pub async fn run_auth(command: AuthCommand, log: Option<&str>) -> anyhow::Result<()> {
    let location = match &command {
        AuthCommand::Pairing { command } => match command {
            PairingCommand::Create { location, .. } | PairingCommand::List { location, .. } | PairingCommand::Revoke { location, .. } => location.clone(),
        },
        AuthCommand::Session { command } => match command {
            SessionCommand::Issue { location, .. } | SessionCommand::List { location, .. } | SessionCommand::Revoke { location, .. } => location.clone(),
        },
    };
    // Flags are validated before anything is opened, like Effect CLI's schema decoding.
    let ttl_ms = match &command {
        AuthCommand::Pairing {
            command: PairingCommand::Create { ttl, .. },
        }
        | AuthCommand::Session {
            command: SessionCommand::Issue { ttl, .. },
        } => parse_ttl(ttl.as_deref())?,
        _ => None,
    };
    let config = resolve_auth_config(&location, log).await?;
    let (_db, auth) = open_environment_auth(&config).await?;
    match command {
        AuthCommand::Pairing { command } => match command {
            PairingCommand::Create {
                label, base_url, json, admin, ..
            } => {
                let issued = auth
                    .create_pairing_link(CreatePairingLinkInput {
                        scopes: Some(if admin {
                            ADMINISTRATIVE_SCOPES.to_vec()
                        } else {
                            STANDARD_CLIENT_SCOPES.to_vec()
                        }),
                        subject: Some("one-time-token".into()),
                        ttl_ms,
                        label,
                        ..CreatePairingLinkInput::default()
                    })
                    .await?;
                print_stdout(&format_issued_pairing_credential(&issued, json, base_url.as_deref()))
            }
            PairingCommand::List { json, .. } => {
                let links = auth.list_pairing_links(Some(&[INTERNAL_ADMINISTRATIVE_BOOTSTRAP_SUBJECT])).await?;
                print_stdout(&format_pairing_credential_list(&links, json))
            }
            PairingCommand::Revoke { id, .. } => {
                let revoked = auth.revoke_pairing_link(&id).await?;
                print_stdout(&format_pairing_revoke(&id, revoked))
            }
        },
        AuthCommand::Session { command } => match command {
            SessionCommand::Issue {
                label,
                subject,
                token_only,
                json,
                ..
            } => {
                let issued = auth
                    .issue_session(IssueBearerSessionInput {
                        scopes: Some(ADMINISTRATIVE_SCOPES.to_vec()),
                        ttl_ms,
                        label,
                        subject,
                    })
                    .await?;
                print_stdout(&format_issued_session(&issued, json, token_only))
            }
            SessionCommand::List { json, .. } => {
                let sessions = auth.list_sessions().await?;
                print_stdout(&format_session_list(&sessions, json))
            }
            SessionCommand::Revoke { session_id, .. } => {
                let session_id = zc_auth::token::js_trim(&session_id).to_owned();
                anyhow::ensure!(!session_id.is_empty(), "Invalid session id: expected a non-empty string");
                let revoked = auth.revoke_session(&session_id).await?;
                print_stdout(&format_session_revoke(&session_id, revoked))
            }
        },
    }
}

pub(crate) fn serve_flags(args: &ServeArgs) -> anyhow::Result<CliServerFlags> {
    Ok(CliServerFlags {
        mode: match args.mode.as_deref() {
            None => None,
            Some(mode) => Some(RuntimeMode::parse(mode).with_context(|| format!("--mode: invalid mode {mode:?}"))?),
        },
        port: args.port,
        host: args.host.clone(),
        base_dir: args.base_dir.clone(),
        cwd: args.cwd.clone(),
        dev_url: args.dev_url.clone(),
        no_browser: args.no_browser.then_some(true),
        auto_bootstrap_project_from_cwd: args.auto_bootstrap_project_from_cwd.then_some(true),
        log_web_socket_events: args.log_websocket_events.then_some(true),
        tailscale_serve_enabled: args.tailscale_serve.then_some(true),
        tailscale_serve_port: args.tailscale_serve_port,
        static_dir: args.static_dir.clone(),
    })
}

/// `serve`'s resolved config (headless presentation).
pub async fn resolve_serve_config(args: &ServeArgs, log: Option<&str>) -> anyhow::Result<ServerConfig> {
    resolve_server_config(
        &serve_flags(args)?,
        log_level(log),
        ResolveServerConfigOptions {
            startup_presentation: StartupPresentation::Headless,
            ..ResolveServerConfigOptions::default()
        },
        &ProcessEnv,
    )
    .await
    .context("could not resolve the server configuration")
}
