//! `project/ProjectSetupScriptRunner.ts`: runs a project's `runOnWorktreeCreate` script in a
//! terminal PTY of the thread, optionally wrapped so the shell echoes a per-run exit sentinel
//! the runner reads back from the terminal stream (plan §6.5).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use futures::StreamExt;
use regex::Regex;
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use zc_contracts::{ProjectId, ProjectScript, ThreadId};
use zc_core::defect::Defect;
use zc_ports::contracts::{TerminalOpenInput, TerminalWriteInput};
use zc_ports::{ProjectionReads, SettingsService, TerminalManager};
use zc_terminal::contracts::{TerminalEvent, TerminalEventKind};

/// The marker the wrapped command echoes with its exit code.
pub const COMPLETION_SENTINEL_PREFIX: &str = "__T3_SETUP_DONE__";
const OUTPUT_LINE_MAX_LENGTH: usize = 400;
/// A partial line longer than this is a byte stream, not a line: only its tail is kept.
const PARTIAL_LINE_MAX_LENGTH: usize = 4_096;

// ---------------------------------------------------------------------------------------------
// `packages/shared/src/projectScripts.ts`

/// `resolveProjectScripts(settings, project)`: the project's override, then the environment
/// defaults; before the legacy fields are folded, the old override map and the aggregate's own
/// scripts still count.
pub fn resolve_project_scripts(settings: &Value, project_id: &str, project_scripts: &[ProjectScript]) -> Vec<ProjectScript> {
    let decode = |value: &Value| serde_json::from_value::<Vec<ProjectScript>>(value.clone()).unwrap_or_default();
    let defaults = || settings.get("defaultProjectScripts").map(decode).unwrap_or_default();
    if let Some(scripts) = settings
        .get("projectSettingsOverrides")
        .and_then(|overrides| overrides.get(project_id))
        .and_then(|entry| entry.get("defaultProjectScripts"))
    {
        return decode(scripts);
    }
    if settings.get("projectSettingsFolded").and_then(Value::as_bool) == Some(true) {
        return defaults();
    }
    match settings.get("projectScriptOverrides").and_then(|overrides| overrides.get(project_id)) {
        Some(Value::Null) => defaults(),
        Some(legacy) => decode(legacy),
        None if !project_scripts.is_empty() => project_scripts.to_vec(),
        None => defaults(),
    }
}

/// `setupProjectScript(scripts)`: the first `runOnWorktreeCreate` script.
pub fn setup_project_script(scripts: &[ProjectScript]) -> Option<&ProjectScript> {
    scripts.iter().find(|script| script.run_on_worktree_create)
}

/// `projectScriptRuntimeEnv({project: {cwd}, worktreePath})`.
pub fn project_script_runtime_env(project_cwd: &str, worktree_path: Option<&str>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("T3CODE_PROJECT_ROOT".to_owned(), project_cwd.to_owned());
    if let Some(worktree) = worktree_path.filter(|w| !w.is_empty()) {
        env.insert("T3CODE_WORKTREE_PATH".to_owned(), worktree.to_owned());
    }
    env
}

// ---------------------------------------------------------------------------------------------
// The completion wrapper

/// The shell the terminal manager will spawn for the setup terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionShell {
    Posix,
    Fish,
    PowerShell,
}

/// `resolveCompletionShell(platform, env)`: `$SHELL` on POSIX, PowerShell on Windows.
pub fn resolve_completion_shell(windows: bool, shell: Option<&str>) -> CompletionShell {
    if windows {
        return CompletionShell::PowerShell;
    }
    let shell = shell.unwrap_or("");
    let name = shell.rsplit('/').next().unwrap_or(shell);
    match name {
        "fish" => CompletionShell::Fish,
        "pwsh" | "powershell" => CompletionShell::PowerShell,
        _ => CompletionShell::Posix,
    }
}

fn completion_sentinel(token: &str) -> String {
    format!("{COMPLETION_SENTINEL_PREFIX}_{token}:")
}

/// `wrapCommandForCompletion`: the command runs in a block closed on its own line (so a
/// trailing comment or heredoc cannot swallow the sentinel), and lines are separated by `\r`,
/// the Enter key of every line editor.
pub fn wrap_command_for_completion(command: &str, shell: CompletionShell, sentinel: &str) -> String {
    let body = command.replace("\r\n", "\r").replace('\n', "\r");
    match shell {
        CompletionShell::PowerShell => format!(
            "$global:LASTEXITCODE = $null; & {{\r{body}\r}}; if ($null -ne $LASTEXITCODE) {{ $__t3c = $LASTEXITCODE }} elseif ($?) {{ $__t3c = 0 }} else {{ $__t3c = 1 }}; Write-Host \"{sentinel}$__t3c\""
        ),
        CompletionShell::Fish => format!("begin\r{body}\rend; printf '\\n{sentinel}%s\\n' $status"),
        CompletionShell::Posix => format!("( {body}\r); printf '\\n{sentinel}%s\\n' \"$?\""),
    }
}

/// `stripTerminalControl`: ANSI escapes and control characters out, so lines read as text.
pub fn strip_terminal_control(text: &str) -> String {
    static ESCAPES: OnceLock<Regex> = OnceLock::new();
    static CONTROLS: OnceLock<Regex> = OnceLock::new();
    let escapes =
        ESCAPES.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][A-Za-z0-9]|\x1b[=>]").expect("valid regex"));
    let controls = CONTROLS.get_or_init(|| Regex::new(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]").expect("valid regex"));
    controls.replace_all(&escapes.replace_all(text, ""), "").into_owned()
}

/// `text.slice(0, n)` in UTF-16 units.
fn js_slice_start(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        text.to_owned()
    } else {
        String::from_utf16_lossy(&units[..max])
    }
}

/// `text.slice(-n)` in UTF-16 units.
fn js_slice_end(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        text.to_owned()
    } else {
        String::from_utf16_lossy(&units[units.len() - max..])
    }
}

/// Splits on `\r\n`, `\r` and `\n`, keeping the trailing partial segment in `buffer`.
fn take_lines(buffer: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = buffer.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines.push(std::mem::take(&mut current));
            }
            '\n' => lines.push(std::mem::take(&mut current)),
            other => current.push(other),
        }
    }
    *buffer = current;
    lines
}

// ---------------------------------------------------------------------------------------------
// Runner

/// `ProjectSetupScriptRunnerInput`.
#[derive(Clone, Default)]
pub struct SetupScriptInput {
    pub thread_id: String,
    pub project_id: Option<String>,
    pub project_cwd: Option<String>,
    pub worktree_path: String,
    pub preferred_terminal_id: Option<String>,
    /// `observeCompletion`: wrap the command with the exit sentinel and forward cleaned output
    /// lines (`onOutputLine`).
    pub observe_completion: Option<ObserveCompletion>,
}

/// `onOutputLine`: receives each cleaned output line of the script.
pub type OutputLineSink = Arc<dyn Fn(&str) + Send + Sync>;

/// `observeCompletion: {onOutputLine?}`.
#[derive(Clone, Default)]
pub struct ObserveCompletion {
    pub on_output_line: Option<OutputLineSink>,
}

/// `ProjectSetupScriptCompletion`: `exitCode` is `None` when the terminal exited or closed
/// before the sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectSetupScriptCompletion {
    pub exit_code: Option<i64>,
    pub duration_ms: u64,
}

/// The `completion` effect: resolves when the sentinel (or an exit/close) arrives; a clean
/// exit closes the setup shell when it is idle.
pub type SetupCompletion = Pin<Box<dyn Future<Output = ProjectSetupScriptCompletion> + Send>>;

/// `ProjectSetupScriptRunnerResultStarted`.
pub struct SetupScriptStarted {
    pub script_id: String,
    pub script_name: String,
    pub script_command: String,
    pub terminal_id: String,
    pub cwd: String,
    /// False when the script's `async` flag asks the agent to wait for it.
    pub r#async: bool,
    pub completion: Option<SetupCompletion>,
}

/// `ProjectSetupScriptRunnerResult`.
pub enum SetupScriptResult {
    NoScript,
    Started(SetupScriptStarted),
}

/// `ProjectSetupScriptOperationError.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupScriptOperation {
    ResolveProject,
    ReadSettings,
    OpenTerminal,
    WriteCommand,
}

impl SetupScriptOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ResolveProject => "resolveProject",
            Self::ReadSettings => "readSettings",
            Self::OpenTerminal => "openTerminal",
            Self::WriteCommand => "writeCommand",
        }
    }
}

/// The context every runner error carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupScriptErrorContext {
    pub thread_id: String,
    pub project_id: Option<String>,
    pub project_cwd: Option<String>,
    pub worktree_path: String,
}

/// `ProjectSetupScriptRunnerError`.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectSetupScriptRunnerError {
    /// `ProjectSetupScriptOperationError`: `cause` is the failing call's error.
    Operation {
        context: SetupScriptErrorContext,
        operation: SetupScriptOperation,
        cause: Defect,
    },
    /// `ProjectSetupScriptProjectNotFoundError`.
    ProjectNotFound { context: SetupScriptErrorContext },
}

impl ProjectSetupScriptRunnerError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Operation { .. } => "ProjectSetupScriptOperationError",
            Self::ProjectNotFound { .. } => "ProjectSetupScriptProjectNotFoundError",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Operation { context, operation, .. } => format!(
                "Project setup script operation '{}' failed for thread '{}' in '{}'.",
                operation.as_str(),
                context.thread_id,
                context.worktree_path
            ),
            Self::ProjectNotFound { context } => format!(
                "Project was not found for setup script execution for thread '{}' in '{}'.",
                context.thread_id, context.worktree_path
            ),
        }
    }

    /// `projectSetupScriptCompatibilityDetail`: what the `setup-script.failed` activity says.
    pub fn compatibility_detail(&self) -> String {
        match self {
            Self::Operation { cause, .. } => cause.message(),
            Self::ProjectNotFound { .. } => "Project was not found for setup script execution.".to_owned(),
        }
    }
}

impl std::fmt::Display for ProjectSetupScriptRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for ProjectSetupScriptRunnerError {}

impl Serialize for ProjectSetupScriptRunnerError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (context, extra) = match self {
            Self::Operation { context, operation, cause } => (context, Some((operation, cause))),
            Self::ProjectNotFound { context } => (context, None),
        };
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("_tag", self.tag())?;
        map.serialize_entry("threadId", &context.thread_id)?;
        if let Some(project_id) = &context.project_id {
            map.serialize_entry("projectId", project_id)?;
        }
        if let Some(project_cwd) = &context.project_cwd {
            map.serialize_entry("projectCwd", project_cwd)?;
        }
        map.serialize_entry("worktreePath", &context.worktree_path)?;
        if let Some((operation, cause)) = extra {
            map.serialize_entry("operation", operation.as_str())?;
            map.serialize_entry("cause", cause)?;
        }
        map.end()
    }
}

/// `ProjectSetupScriptRunner` as a trait: the bootstrap's tests stand in for it.
#[async_trait]
pub trait SetupScriptRunner: Send + Sync {
    /// `runForThread(input)`.
    async fn run_for_thread(&self, input: SetupScriptInput) -> Result<SetupScriptResult, ProjectSetupScriptRunnerError>;
}

/// The live runner.
#[derive(Clone)]
pub struct ProjectSetupScriptRunner {
    projections: Arc<dyn ProjectionReads>,
    terminals: Arc<dyn TerminalManager>,
    settings: Arc<dyn SettingsService>,
    shell: CompletionShell,
}

impl ProjectSetupScriptRunner {
    /// With the completion shell predicted from this process (`$SHELL`, the platform).
    pub fn new(projections: Arc<dyn ProjectionReads>, terminals: Arc<dyn TerminalManager>, settings: Arc<dyn SettingsService>) -> Self {
        let shell = resolve_completion_shell(cfg!(windows), std::env::var("SHELL").ok().as_deref());
        Self::with_shell(projections, terminals, settings, shell)
    }

    pub fn with_shell(
        projections: Arc<dyn ProjectionReads>,
        terminals: Arc<dyn TerminalManager>,
        settings: Arc<dyn SettingsService>,
        shell: CompletionShell,
    ) -> Self {
        Self {
            projections,
            terminals,
            settings,
            shell,
        }
    }

    /// `observeTerminalCompletion`: subscribes now; the returned receiver settles on the
    /// sentinel, an exit or a close. Dropping `stop` tears the subscription down.
    fn observe(
        &self,
        thread_id: String,
        terminal_id: String,
        sentinel: String,
        echoed_wrapper_lines: Vec<String>,
        on_output_line: Option<OutputLineSink>,
    ) -> (oneshot::Receiver<ProjectSetupScriptCompletion>, oneshot::Sender<()>) {
        let mut events = self.terminals.subscribe();
        let (done, completion) = oneshot::channel();
        let (stop, mut stopped) = oneshot::channel::<()>();
        let pattern = Regex::new(&format!("{}(-?\\d+)", regex::escape(&sentinel))).expect("valid regex");
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            let mut buffer = String::new();
            let settle = |exit_code: Option<i64>| ProjectSetupScriptCompletion {
                exit_code,
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            };
            loop {
                let event = tokio::select! {
                    _ = &mut stopped => return,
                    event = events.next() => event,
                };
                let Some(event) = event else { return };
                let Ok(event) = serde_json::from_value::<TerminalEvent>(event.0) else {
                    continue;
                };
                if event.thread_id != thread_id || event.terminal_id != terminal_id {
                    continue;
                }
                match event.kind {
                    TerminalEventKind::Output { data } => {
                        buffer.push_str(&data);
                        // A bare carriage return redraws a progress line: each redraw becomes
                        // its own short line.
                        let lines = take_lines(&mut buffer);
                        // The sentinel is always on its own line, so keeping the tail is safe.
                        if buffer.encode_utf16().count() > PARTIAL_LINE_MAX_LENGTH {
                            buffer = js_slice_end(&buffer, PARTIAL_LINE_MAX_LENGTH);
                        }
                        for raw in lines {
                            if let Some(captures) = pattern.captures(&raw) {
                                let exit_code = captures[1].parse::<i64>().ok();
                                let _ = done.send(settle(exit_code));
                                return;
                            }
                            let cleaned = strip_terminal_control(&raw);
                            let cleaned = cleaned.trim_end();
                            if cleaned.is_empty() || cleaned.contains(&sentinel) || echoed_wrapper_lines.iter().any(|echoed| cleaned.ends_with(echoed.as_str()))
                            {
                                continue;
                            }
                            if let Some(on_output_line) = &on_output_line {
                                on_output_line(&js_slice_start(cleaned, OUTPUT_LINE_MAX_LENGTH));
                            }
                        }
                    }
                    TerminalEventKind::Exited { .. } | TerminalEventKind::Closed => {
                        let _ = done.send(settle(None));
                        return;
                    }
                    _ => {}
                }
            }
        });
        (completion, stop)
    }

    async fn resolve_project(
        &self,
        input: &SetupScriptInput,
        context: &SetupScriptErrorContext,
    ) -> Result<Option<(String, String, Vec<ProjectScript>)>, ProjectSetupScriptRunnerError> {
        let operation_error = |cause: zc_ports::TaggedError| ProjectSetupScriptRunnerError::Operation {
            context: context.clone(),
            operation: SetupScriptOperation::ResolveProject,
            cause: Defect::error(&cause.tag, cause.to_string()),
        };
        if let Some(project_id) = input.project_id.as_deref().filter(|id| !id.is_empty()) {
            if let Some(project) = self
                .projections
                .get_project_shell_by_id(&ProjectId::new(project_id))
                .await
                .map_err(operation_error)?
            {
                return Ok(Some((project.id.0, project.workspace_root, project.scripts)));
            }
        }
        if let Some(project_cwd) = input.project_cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
            if let Some(project) = self
                .projections
                .get_active_project_by_workspace_root(project_cwd)
                .await
                .map_err(operation_error)?
            {
                return Ok(Some((project.id.0, project.workspace_root, project.scripts)));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl SetupScriptRunner for ProjectSetupScriptRunner {
    async fn run_for_thread(&self, input: SetupScriptInput) -> Result<SetupScriptResult, ProjectSetupScriptRunnerError> {
        let context = SetupScriptErrorContext {
            thread_id: input.thread_id.clone(),
            project_id: input.project_id.clone(),
            project_cwd: input.project_cwd.clone(),
            worktree_path: input.worktree_path.clone(),
        };
        let Some((project_id, workspace_root, project_scripts)) = self.resolve_project(&input, &context).await? else {
            return Err(ProjectSetupScriptRunnerError::ProjectNotFound { context });
        };

        let settings = self.settings.get_settings().await.map_err(|cause| ProjectSetupScriptRunnerError::Operation {
            context: context.clone(),
            operation: SetupScriptOperation::ReadSettings,
            cause: Defect::error("ServerSettingsError", zc_settings::errors::settings_error_message(&cause)),
        })?;
        let settings = serde_json::to_value(&settings).unwrap_or(Value::Null);
        let scripts = resolve_project_scripts(&settings, &project_id, &project_scripts);
        let Some(script) = setup_project_script(&scripts).cloned() else {
            return Ok(SetupScriptResult::NoScript);
        };

        let terminal_id = input.preferred_terminal_id.clone().unwrap_or_else(|| format!("setup-{}", script.id));
        let cwd = input.worktree_path.clone();
        let mut env = project_script_runtime_env(&workspace_root, Some(&input.worktree_path));
        // Setup may run before a terminal client attaches to answer color probes.
        env.insert("NO_COLOR".into(), "1".into());
        env.insert("FORCE_COLOR".into(), "0".into());
        let token = input.observe_completion.as_ref().map(|_| uuid::Uuid::new_v4().simple().to_string());
        let command_line = match &token {
            Some(token) => wrap_command_for_completion(&script.command, self.shell, &completion_sentinel(token)),
            None => script.command.clone(),
        };

        self.terminals
            .open(TerminalOpenInput(json!({
                "threadId": input.thread_id,
                "terminalId": terminal_id,
                "cwd": cwd,
                "worktreePath": input.worktree_path,
                "env": env,
            })))
            .await
            .map_err(|cause| ProjectSetupScriptRunnerError::Operation {
                context: context.clone(),
                operation: SetupScriptOperation::OpenTerminal,
                cause: Defect::error(&cause.tag, cause.to_string()),
            })?;

        // Subscribe before writing so the sentinel cannot race past the listener.
        let observed = match (&input.observe_completion, &token) {
            (Some(observe), Some(token)) => Some(self.observe(
                input.thread_id.clone(),
                terminal_id.clone(),
                completion_sentinel(token),
                command_line.split('\r').filter(|line| !line.is_empty()).map(str::to_owned).collect(),
                observe.on_output_line.clone(),
            )),
            _ => None,
        };

        let written = self
            .terminals
            .write(TerminalWriteInput(json!({
                "threadId": input.thread_id,
                "terminalId": terminal_id,
                "data": format!("{command_line}\r"),
            })))
            .await;
        let observed = match written {
            Ok(()) => observed,
            Err(cause) => {
                // Nothing will ever settle the completion if the command never ran.
                drop(observed);
                return Err(ProjectSetupScriptRunnerError::Operation {
                    context,
                    operation: SetupScriptOperation::WriteCommand,
                    cause: Defect::error(&cause.tag, cause.to_string()),
                });
            }
        };

        // A clean run leaves only an idle prompt: close it. A failed run keeps its shell.
        let completion = observed.map(|(receiver, stop)| {
            let terminals = self.terminals.clone();
            let thread_id = ThreadId::new(input.thread_id.clone());
            let terminal_id = terminal_id.clone();
            Box::pin(async move {
                let completion = receiver.await.unwrap_or(ProjectSetupScriptCompletion {
                    exit_code: None,
                    duration_ms: 0,
                });
                drop(stop);
                if completion.exit_code == Some(0) {
                    terminals.close_idle(&thread_id, Some(&terminal_id)).await;
                }
                completion
            }) as SetupCompletion
        });

        Ok(SetupScriptResult::Started(SetupScriptStarted {
            script_id: script.id.clone(),
            script_name: script.name.clone(),
            script_command: script.command.clone(),
            terminal_id,
            cwd,
            r#async: script.r#async != Some(false),
            completion,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_match_the_ts_shell_syntax() {
        let sentinel = completion_sentinel(&"0".repeat(32));
        assert_eq!(
            wrap_command_for_completion("bun install", CompletionShell::Posix, &sentinel),
            format!("( bun install\r); printf '\\n{sentinel}%s\\n' \"$?\"")
        );
        assert_eq!(
            wrap_command_for_completion("bun install", CompletionShell::Fish, &sentinel),
            format!("begin\rbun install\rend; printf '\\n{sentinel}%s\\n' $status")
        );
        assert!(wrap_command_for_completion("a\nb", CompletionShell::PowerShell, &sentinel).starts_with("$global:LASTEXITCODE = $null; & {\ra\rb\r};"));
        assert_eq!(resolve_completion_shell(false, Some("/usr/bin/fish")), CompletionShell::Fish);
        assert_eq!(resolve_completion_shell(false, Some("/bin/zsh")), CompletionShell::Posix);
        assert_eq!(resolve_completion_shell(false, Some("/usr/local/bin/pwsh")), CompletionShell::PowerShell);
        assert_eq!(resolve_completion_shell(true, Some("/bin/zsh")), CompletionShell::PowerShell);
    }

    #[test]
    fn lines_split_on_every_line_ending_and_controls_are_stripped() {
        let mut buffer = "a\r\nb\rc\nd".to_owned();
        assert_eq!(take_lines(&mut buffer), vec!["a", "b", "c"]);
        assert_eq!(buffer, "d");
        assert_eq!(strip_terminal_control("\u{1b}[32mResolving deps\u{1b}[0m"), "Resolving deps");
        assert_eq!(strip_terminal_control("\u{1b}]0;title\u{7}x\u{8}"), "x");
    }

    #[test]
    fn project_scripts_resolve_like_the_shared_helper() {
        let script = |id: &str| -> ProjectScript {
            serde_json::from_value(json!({"id": id, "name": id, "command": "x", "icon": "configure", "runOnWorktreeCreate": true})).unwrap()
        };
        let own = vec![script("own")];
        let settings = json!({"defaultProjectScripts": [script("default")], "projectScriptOverrides": {}, "projectSettingsOverrides": {}});
        assert_eq!(resolve_project_scripts(&settings, "p", &own)[0].id, "own");
        assert_eq!(resolve_project_scripts(&settings, "p", &[])[0].id, "default");
        let legacy_null = json!({"defaultProjectScripts": [script("default")], "projectScriptOverrides": {"p": null}});
        assert_eq!(resolve_project_scripts(&legacy_null, "p", &own)[0].id, "default");
        let folded = json!({"defaultProjectScripts": [script("default")], "projectSettingsFolded": true});
        assert_eq!(resolve_project_scripts(&folded, "p", &own)[0].id, "default");
        let overridden = json!({"projectSettingsOverrides": {"p": {"defaultProjectScripts": [script("override")]}}, "projectSettingsFolded": true});
        assert_eq!(resolve_project_scripts(&overridden, "p", &own)[0].id, "override");
        assert_eq!(project_script_runtime_env("/r", None).len(), 1);
    }
}
