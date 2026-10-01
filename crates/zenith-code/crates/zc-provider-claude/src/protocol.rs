//! The `claude` CLI in stream-json mode, spoken directly: the port of the SDK's
//! `ProcessTransport` (spawn, stdin/stdout NDJSON, stderr tail, exit errors, close escalation)
//! and `Query` (control requests both ways, `initialize`, the message stream).
//!
//! Wire shapes (all one JSON object per line):
//! - out: `{"type":"control_request","request_id","request":{"subtype":…}}` (`initialize` first,
//!   then `interrupt`, `set_model`, `set_permission_mode`, `get_usage`), user messages,
//!   `{"type":"control_response","response":{"subtype":"success"|"error","request_id",…}}`;
//! - in: `control_response`, `control_request` (`can_use_tool`, `request_user_dialog`,
//!   `elicitation`, …), `control_cancel_request`, `keep_alive`, and SDK messages.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rand::Rng;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::options::{build_spawn_spec, initialize_request, is_native_executable, ClaudeQueryOptions};
use crate::query::{
    CanUseToolRequest, ClaudeQueryFactory, ClaudeQueryHandle, ClaudeQueryRuntime, MessageReceiver, PromptReceiver, QueryCallbacks, QueryError,
    UserDialogRequest,
};

/// Stderr kept for exit errors (`TR`).
const STDERR_TAIL_CHARS: usize = 2048;
/// `close()` waits this long before SIGTERM (`dze`).
const CLOSE_GRACE: Duration = Duration::from_millis(2000);
/// …and this long after SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(5000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExitInfo {
    code: Option<i32>,
    signal: Option<i32>,
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, QueryError>>>;

struct Shared {
    writer: Mutex<Option<mpsc::UnboundedSender<String>>>,
    pending: Mutex<Pending>,
    inbound: Mutex<HashMap<String, CancellationToken>>,
    messages: Mutex<Option<mpsc::UnboundedSender<Result<Value, QueryError>>>>,
    closed: AtomicBool,
    pid: Option<u32>,
    exit: watch::Receiver<Option<ExitInfo>>,
    init: watch::Receiver<Option<Result<Value, QueryError>>>,
    first_result: Notify,
    first_result_seen: AtomicBool,
    callbacks: Option<Arc<dyn QueryCallbacks>>,
    stderr_tail: Mutex<String>,
    last_error_result_text: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `Math.random().toString(36).substring(2, 15)`.
fn random_request_id() -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    (0..11).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect()
}

impl Shared {
    fn write_line(&self, line: String) -> Result<(), QueryError> {
        match lock(&self.writer).as_ref() {
            Some(writer) => writer.send(line).map_err(|_| QueryError::new("Cannot write to terminated process")),
            None => Err(QueryError::new("ProcessTransport is not ready for writing")),
        }
    }

    fn end_input(&self) {
        lock(&self.writer).take();
    }

    fn emit(&self, item: Result<Value, QueryError>) {
        if let Some(sender) = lock(&self.messages).as_ref() {
            let _ = sender.send(item);
        }
    }

    fn reject_pending(&self, error: &QueryError) {
        for (_, sender) in lock(&self.pending).drain() {
            let _ = sender.send(Err(error.clone()));
        }
    }

    fn formatted_stderr_tail(&self) -> String {
        let tail = lock(&self.stderr_tail);
        let chars: Vec<char> = tail.chars().collect();
        let start = chars.len().saturating_sub(STDERR_TAIL_CHARS);
        let text: String = chars[start..].iter().collect();
        let text = text.trim();
        if text.is_empty() {
            String::new()
        } else {
            format!(". stderr: {text}")
        }
    }

    fn exit_error(&self, info: ExitInfo) -> Option<QueryError> {
        if let Some(code) = info.code.filter(|code| *code != 0) {
            return Some(QueryError::new(format!(
                "Claude Code process exited with code {code}{}",
                self.formatted_stderr_tail()
            )));
        }
        if let Some(signal) = info.signal {
            return Some(QueryError::new(format!(
                "Claude Code process terminated by signal {}{}",
                zc_core::process::signal_name(signal),
                self.formatted_stderr_tail()
            )));
        }
        None
    }

    async fn request(self: &Arc<Self>, request: Value) -> Result<Value, QueryError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(QueryError::new("Query closed before response received"));
        }
        let request_id = random_request_id();
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(request_id.clone(), sender);
        let line = json!({ "request_id": request_id, "type": "control_request", "request": request }).to_string();
        if let Err(error) = self.write_line(line) {
            lock(&self.pending).remove(&request_id);
            return Err(error);
        }
        receiver.await.unwrap_or_else(|_| Err(QueryError::new("Query closed before response received")))
    }

    fn on_control_response(&self, response: &Value) {
        let Some(request_id) = response.get("request_id").and_then(Value::as_str) else {
            return;
        };
        let Some(sender) = lock(&self.pending).remove(request_id) else { return };
        let result = if response.get("subtype").and_then(Value::as_str) == Some("success") {
            Ok(response.get("response").cloned().unwrap_or(Value::Null))
        } else {
            Err(QueryError::new(
                response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Claude Code control request failed")
                    .to_string(),
            ))
        };
        let _ = sender.send(result);
    }

    fn send_control_response(&self, request_id: &str, result: Result<Value, String>) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        let response = match result {
            Ok(body) => json!({ "type": "control_response", "response": { "subtype": "success", "request_id": request_id, "response": body } }),
            Err(error) => json!({ "type": "control_response", "response": { "subtype": "error", "request_id": request_id, "error": error } }),
        };
        let _ = self.write_line(response.to_string());
    }

    /// `Query.handleControlRequest` + `processControlRequest`.
    fn on_control_request(self: &Arc<Self>, message: Value) {
        let Some(request_id) = message.get("request_id").and_then(Value::as_str).map(str::to_string) else {
            return;
        };
        let request = message.get("request").cloned().unwrap_or(Value::Null);
        let token = {
            let mut inbound = lock(&self.inbound);
            if inbound.contains_key(&request_id) {
                tracing::debug!(request_id, "duplicate delivery of an in-flight Claude control request");
                return;
            }
            let token = CancellationToken::new();
            inbound.insert(request_id.clone(), token.clone());
            token
        };
        let shared = self.clone();
        tokio::spawn(async move {
            let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or_default().to_string();
            let str_field = |key: &str| request.get(key).and_then(Value::as_str).map(str::to_string);
            let outcome: Option<Result<Value, String>> = match subtype.as_str() {
                "can_use_tool" => match &shared.callbacks {
                    None => Some(Err("canUseTool callback is not provided.".into())),
                    Some(callbacks) => {
                        let tool_use_id = str_field("tool_use_id");
                        let input = CanUseToolRequest {
                            tool_name: str_field("tool_name").unwrap_or_default(),
                            input: request.get("input").cloned().unwrap_or(Value::Null),
                            suggestions: request.get("permission_suggestions").and_then(Value::as_array).cloned(),
                            tool_use_id: tool_use_id.clone(),
                            agent_id: str_field("agent_id"),
                            request_id: request_id.clone(),
                        };
                        let result = callbacks.can_use_tool(input, token.clone()).await;
                        Some(Ok(result.to_response(tool_use_id.as_deref())))
                    }
                },
                "request_user_dialog" => match &shared.callbacks {
                    None => None,
                    Some(callbacks) => {
                        let input = UserDialogRequest {
                            dialog_kind: str_field("dialog_kind").unwrap_or_default(),
                            payload: request.get("payload").cloned().unwrap_or(Value::Null),
                            tool_use_id: str_field("tool_use_id"),
                            request_id: request_id.clone(),
                        };
                        callbacks.on_user_dialog(input, token.clone()).await.map(Ok)
                    }
                },
                "elicitation" => Some(Ok(json!({ "action": "decline" }))),
                "hook_callback" => Some(Err(format!("No hook callback found for ID: {}", str_field("callback_id").unwrap_or_default()))),
                "mcp_message" => Some(Err(format!("SDK MCP server not found: {}", str_field("server_name").unwrap_or_default()))),
                "oauth_token_refresh" => Some(Err("getOAuthToken callback is not provided.".into())),
                "host_auth_token_refresh" => Some(Err("getHostAuthToken callback is not provided.".into())),
                "remote_control_work_secret" => Some(Err("refreshWorkSecret callback is not provided.".into())),
                other => Some(Err(format!("Unsupported control request subtype: {other}"))),
            };
            if let Some(outcome) = outcome {
                shared.send_control_response(&request_id, outcome);
            }
            lock(&shared.inbound).remove(&request_id);
        });
    }

    fn on_control_cancel(&self, message: &Value) {
        if let Some(request_id) = message.get("request_id").and_then(Value::as_str) {
            if let Some(token) = lock(&self.inbound).remove(request_id) {
                token.cancel();
            }
        }
    }

    /// Track the last error result (the SDK reports it instead of a generic exit error).
    fn track_result_text(&self, message: &Value) {
        let kind = message.get("type").and_then(Value::as_str);
        if kind == Some("result") {
            let text = if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                if message.get("subtype").and_then(Value::as_str) == Some("success") {
                    message.get("result").and_then(Value::as_str).map(str::to_string)
                } else {
                    message.get("errors").and_then(Value::as_array).map(|errors| {
                        errors
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::trim)
                            .filter(|e| !e.is_empty())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                }
            } else {
                None
            };
            *lock(&self.last_error_result_text) = text.filter(|t| !t.is_empty());
            self.first_result_seen.store(true, Ordering::SeqCst);
            self.first_result.notify_waiters();
        } else if !(kind == Some("system") && message.get("subtype").and_then(Value::as_str) == Some("session_state_changed")) {
            *lock(&self.last_error_result_text) = None;
        }
    }

    fn close(self: &Arc<Self>) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        for (_, token) in lock(&self.inbound).drain() {
            token.cancel();
        }
        self.reject_pending(&QueryError::new("Query closed before response received"));
        self.end_input();
        lock(&self.messages).take();
        self.first_result.notify_waiters();
        if let Some(pid) = self.pid {
            if self.exit.borrow().is_none() {
                let mut exit = self.exit.clone();
                tokio::spawn(async move {
                    if tokio::time::timeout(CLOSE_GRACE, exit.wait_for(Option::is_some)).await.is_ok() {
                        return;
                    }
                    send_signal(pid, libc::SIGTERM);
                    if tokio::time::timeout(KILL_GRACE, exit.wait_for(Option::is_some)).await.is_err() {
                        send_signal(pid, libc::SIGKILL);
                    }
                });
            }
        }
    }
}

fn send_signal(pid: u32, signal: i32) {
    #[cfg(unix)]
    // SAFETY: plain kill(2) on the child we spawned; a stale pid only yields ESRCH.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
    #[cfg(not(unix))]
    let _ = (pid, signal);
}

/// A running `claude` process speaking stream-json.
#[derive(Clone)]
pub struct ProcessQuery {
    shared: Arc<Shared>,
}

impl ProcessQuery {
    /// Spawn the CLI as the SDK would for `options` and start the protocol. `prompt` feeds user
    /// messages (`None`: a prompt that never yields, like the status probe's). Spawn failures
    /// surface as the message stream's error, as in the SDK.
    pub fn spawn(options: &ClaudeQueryOptions, prompt: Option<PromptReceiver>, callbacks: Option<Arc<dyn QueryCallbacks>>) -> (Self, MessageReceiver) {
        let spec = build_spawn_spec(options);
        let (messages_tx, messages_rx) = mpsc::unbounded_channel();
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .env_clear()
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        if let Some(cwd) = &spec.cwd {
            command.current_dir(cwd);
        }
        let (exit_tx, exit_rx) = watch::channel(None);
        let (init_tx, init_rx) = watch::channel(None);
        let spawned = command.spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                let native = is_native_executable(&options.path_to_claude_code_executable);
                let message = if error.kind() == std::io::ErrorKind::NotFound {
                    if native {
                        format!(
                            "Claude Code native binary not found at {}. Please ensure Claude Code is installed via native installer or specify a valid path with options.pathToClaudeCodeExecutable.",
                            options.path_to_claude_code_executable
                        )
                    } else {
                        format!(
                            "Claude Code executable not found at {}. Is options.pathToClaudeCodeExecutable set?",
                            options.path_to_claude_code_executable
                        )
                    }
                } else {
                    format!("Failed to spawn Claude Code process: {error}")
                };
                let _ = messages_tx.send(Err(QueryError::new(message.clone())));
                let _ = exit_tx.send(Some(ExitInfo { code: None, signal: None }));
                let _ = init_tx.send(Some(Err(QueryError::new(message))));
                let shared = Arc::new(Shared {
                    writer: Mutex::new(None),
                    pending: Mutex::default(),
                    inbound: Mutex::default(),
                    messages: Mutex::new(None),
                    closed: AtomicBool::new(false),
                    pid: None,
                    exit: exit_rx,
                    init: init_rx,
                    first_result: Notify::new(),
                    first_result_seen: AtomicBool::new(false),
                    callbacks,
                    stderr_tail: Mutex::default(),
                    last_error_result_text: Mutex::default(),
                });
                return (Self { shared }, messages_rx);
            }
        };
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<String>();
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(writer_tx)),
            pending: Mutex::default(),
            inbound: Mutex::default(),
            messages: Mutex::new(Some(messages_tx)),
            closed: AtomicBool::new(false),
            pid,
            exit: exit_rx,
            init: init_rx,
            first_result: Notify::new(),
            first_result_seen: AtomicBool::new(false),
            callbacks,
            stderr_tail: Mutex::default(),
            last_error_result_text: Mutex::default(),
        });

        // stdin writer: lines in order; EOF when every sender is gone.
        if let Some(mut stdin) = stdin {
            tokio::spawn(async move {
                while let Some(line) = writer_rx.recv().await {
                    if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                        break;
                    }
                    let _ = stdin.flush().await;
                }
                let _ = stdin.shutdown().await;
            });
        }

        // Process exit.
        tokio::spawn(async move {
            let info = match child.wait().await {
                Ok(status) => {
                    #[cfg(unix)]
                    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                    #[cfg(not(unix))]
                    let signal = None;
                    ExitInfo { code: status.code(), signal }
                }
                Err(_) => ExitInfo { code: None, signal: None },
            };
            let _ = exit_tx.send(Some(info));
        });

        // stderr tail.
        if let Some(mut stderr) = stderr {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                loop {
                    match stderr.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let mut tail = lock(&shared.stderr_tail);
                            tail.push_str(&String::from_utf8_lossy(&buffer[..n]));
                            if tail.chars().count() > 2 * STDERR_TAIL_CHARS {
                                let chars: Vec<char> = tail.chars().collect();
                                *tail = chars[chars.len() - STDERR_TAIL_CHARS..].iter().collect();
                            }
                        }
                    }
                }
            });
        }

        // initialize, written before any user message.
        {
            let pending = shared.queue_request(initialize_request(options));
            let init_shared = shared.clone();
            tokio::spawn(async move {
                let result = pending.await;
                if let Ok(response) = &result {
                    init_shared.redeliver_pending_prompts(response);
                }
                let _ = init_tx.send(Some(result));
            });
            tokio::spawn(shared.clone().run_prompt(prompt));
        }

        // stdout reader.
        if let Some(stdout) = stdout {
            let shared = shared.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let Ok(message) = serde_json::from_str::<Value>(&line) else {
                        tracing::debug!("Non-JSON stdout from Claude Code");
                        continue;
                    };
                    match message.get("type").and_then(Value::as_str) {
                        Some("control_response") => {
                            if let Some(response) = message.get("response") {
                                shared.on_control_response(response);
                            }
                        }
                        Some("control_request") => shared.on_control_request(message),
                        Some("control_cancel_request") => shared.on_control_cancel(&message),
                        Some("keep_alive") | Some("transcript_mirror") => {}
                        _ => {
                            shared.track_result_text(&message);
                            shared.emit(Ok(message));
                        }
                    }
                }
                // stdout closed: settle like `readMessages` + `waitForExit`.
                let mut exit = shared.exit.clone();
                let info = exit.wait_for(Option::is_some).await.ok().and_then(|info| *info);
                let error = info
                    .and_then(|info| shared.exit_error(info))
                    .map(|error| match lock(&shared.last_error_result_text).clone() {
                        Some(text) => QueryError::new(format!("Claude Code returned an error result: {text}")),
                        None => error,
                    });
                if let Some(error) = &error {
                    if !shared.closed.load(Ordering::SeqCst) {
                        shared.emit(Err(error.clone()));
                    }
                }
                shared.reject_pending(&error.unwrap_or_else(|| QueryError::new("Query closed before response received")));
                lock(&shared.messages).take();
                shared.first_result.notify_waiters();
            });
        }

        (Self { shared }, messages_rx)
    }

    /// The `initialize` response (`initializationResult()`: commands, models, account, …).
    pub async fn initialization_result(&self) -> Result<Value, QueryError> {
        let mut init = self.shared.init.clone();
        let result = match init.wait_for(Option::is_some).await {
            Ok(value) => value.clone().unwrap_or_else(|| Err(QueryError::new("Query closed before response received"))),
            Err(_) => Err(QueryError::new("Query closed before response received")),
        };
        result
    }

    /// `usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET()`: the `get_usage` response.
    pub async fn get_usage(&self) -> Result<Value, QueryError> {
        self.shared.request(json!({ "subtype": "get_usage" })).await
    }

    /// Send an arbitrary control request (`{subtype, …}`) and await its response body.
    pub async fn control_request(&self, request: Value) -> Result<Value, QueryError> {
        self.shared.request(request).await
    }

    /// Whether the process has exited.
    pub fn has_exited(&self) -> bool {
        self.shared.exit.borrow().is_some()
    }
}

impl Shared {
    /// `request()` whose line is written now (before any user message) and answered later.
    fn queue_request(self: &Arc<Self>, request: Value) -> impl std::future::Future<Output = Result<Value, QueryError>> + Send + 'static {
        let request_id = random_request_id();
        let (sender, receiver) = oneshot::channel();
        lock(&self.pending).insert(request_id.clone(), sender);
        let line = json!({ "request_id": request_id, "type": "control_request", "request": request }).to_string();
        let written = self.write_line(line);
        let shared = self.clone();
        async move {
            if let Err(error) = written {
                lock(&shared.pending).remove(&request_id);
                return Err(error);
            }
            receiver.await.unwrap_or_else(|_| Err(QueryError::new("Query closed before response received")))
        }
    }

    /// `pending_permission_requests` / `pending_user_dialog_requests` on the `initialize`
    /// response: prompts the CLI re-delivers after a resume.
    fn redeliver_pending_prompts(self: &Arc<Self>, response: &Value) {
        for (key, subtype) in [
            ("pending_permission_requests", "can_use_tool"),
            ("pending_user_dialog_requests", "request_user_dialog"),
        ] {
            for request in response.get(key).and_then(Value::as_array).into_iter().flatten() {
                if request.get("request").and_then(|r| r.get("subtype")).and_then(Value::as_str) == Some(subtype) {
                    self.on_control_request(request.clone());
                }
            }
        }
    }

    /// `streamInput`: user messages in order; once the prompt ends, wait for the first result
    /// (the adapter always has bidirectional needs), then close stdin.
    async fn run_prompt(self: Arc<Self>, prompt: Option<PromptReceiver>) {
        let Some(mut prompt) = prompt else { return };
        let mut count = 0usize;
        while let Some(message) = prompt.recv().await {
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            count += 1;
            if self.write_line(message.to_string()).is_err() {
                return;
            }
        }
        if count > 0 && self.callbacks.is_some() {
            loop {
                let notified = self.first_result.notified();
                if self.first_result_seen.load(Ordering::SeqCst) || self.closed.load(Ordering::SeqCst) || lock(&self.messages).is_none() {
                    break;
                }
                notified.await;
            }
        }
        self.end_input();
    }
}

#[async_trait]
impl ClaudeQueryRuntime for ProcessQuery {
    async fn interrupt(&self) -> Result<(), QueryError> {
        self.shared.request(json!({ "subtype": "interrupt" })).await.map(|_| ())
    }

    async fn set_model(&self, model: Option<&str>) -> Result<(), QueryError> {
        let mut request = Map::new();
        request.insert("subtype".into(), Value::String("set_model".into()));
        if let Some(model) = model {
            request.insert("model".into(), Value::String(model.to_string()));
        }
        self.shared.request(Value::Object(request)).await.map(|_| ())
    }

    async fn set_permission_mode(&self, mode: &str) -> Result<(), QueryError> {
        self.shared.request(json!({ "subtype": "set_permission_mode", "mode": mode })).await.map(|_| ())
    }

    fn close(&self) -> Result<(), QueryError> {
        self.shared.close();
        Ok(())
    }
}

/// The real [`ClaudeQueryFactory`]: spawns the user's `claude`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcessQueryFactory;

impl ClaudeQueryFactory for ProcessQueryFactory {
    fn create(&self, options: ClaudeQueryOptions, prompt: PromptReceiver, callbacks: Arc<dyn QueryCallbacks>) -> Result<ClaudeQueryHandle, QueryError> {
        let (query, messages) = ProcessQuery::spawn(&options, Some(prompt), Some(callbacks));
        Ok(ClaudeQueryHandle {
            runtime: Arc::new(query),
            messages,
        })
    }
}
