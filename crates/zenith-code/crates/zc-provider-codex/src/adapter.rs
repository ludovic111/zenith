//! The Codex provider adapter (`provider/Layers/CodexAdapter.ts`): one session runtime per
//! thread, its native events mapped to canonical runtime events, runtime failures mapped to the
//! shared adapter errors. Implements [`zc_ports::adapter::ProviderAdapter`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::Value;
use tokio::task::AbortHandle;
use zc_contracts::{
    ApprovalRequestId, ChatAttachment, CodexSettings, ModelSelection, ProviderApprovalDecision, ProviderDriverKind, ProviderEvent, ProviderInstanceId,
    ProviderRuntimeEvent, ProviderSendTurnInput, ProviderSession, ProviderSessionStartInput, ProviderSetupError, ProviderTurnStartResult,
    ProviderUploadFeedbackInput, ProviderUploadFeedbackResult, ProviderUserInputAnswers, ThreadId, TurnId,
};
use zc_core::PubSub;
use zc_ports::adapter::{
    AdapterCapabilities, AdapterError, AdapterResult, Compaction, ProviderAdapter, SessionModelSwitch, ThreadSnapshot, ThreadTurnSnapshot,
};

use crate::errors::{CodexAppServerError, CodexSessionRuntimeError};
use crate::launch_args::{resolve_codex_launch_args, Environment};
use crate::managed::CodexEffectiveRuntime;
use crate::mapping::{to_runtime_event, CodexEventMapper};
use crate::model::{codex_service_tier_option_value, model_selection_string_option};
use crate::session_runtime::{CodexRuntime, CodexSessionRuntime, CodexSessionRuntimeOptions, ModelsSource, SendTurnInput};
use crate::thread_history::CodexThreadSnapshot;

fn provider() -> String {
    crate::DRIVER_KIND.to_owned()
}

/// Builds a session runtime (`makeRuntime`); tests inject fakes.
pub type RuntimeFactory = Arc<dyn Fn(CodexSessionRuntimeOptions) -> BoxFuture<'static, Result<Arc<dyn CodexRuntime>, CodexAppServerError>> + Send + Sync>;

/// Resolves the managed runtime per session (`resolveRuntime`).
pub type ResolveRuntime = Arc<dyn Fn() -> BoxFuture<'static, Result<CodexEffectiveRuntime, ProviderSetupError>> + Send + Sync>;

/// Called when a managed sharing error says the connection is gone.
pub type OnRevoked = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Receives every native event (the `NTIVE:` log, `EventNdjsonLogger` in WP-12).
pub trait NativeEventSink: Send + Sync {
    fn write(&self, event: &ProviderEvent);
}

/// A thread's `t3-code` MCP session (`McpProviderSession.ts`, WP-27a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProviderSession {
    pub endpoint: String,
    pub authorization_header: String,
    pub capabilities: BTreeSet<String>,
    /// Extra environment for the `agent-device` CLI (its `PATH` entry is prepended).
    pub agent_device_environment: Option<BTreeMap<String, String>>,
}

/// Looks up the MCP session registered for a thread (`readMcpProviderSession`).
pub trait McpSessionLookup: Send + Sync {
    fn read(&self, thread_id: &ThreadId) -> Option<McpProviderSession>;
}

/// `withAgentDeviceEnvironment`: the device variables over `base`, with the shim directory
/// prepended to `PATH`.
pub fn with_agent_device_environment(base: &Environment, session: &McpProviderSession) -> Environment {
    let Some(extra) = &session.agent_device_environment else {
        return base.clone();
    };
    let separator = extra.get("PATH_SEPARATOR").cloned().unwrap_or_else(|| ":".to_owned());
    let base_path = base.get("PATH").or_else(|| base.get("Path")).cloned();
    let mut out = base.clone();
    for (key, value) in extra {
        if key != "PATH" && key != "PATH_SEPARATOR" {
            out.insert(key.clone(), value.clone());
        }
    }
    if let Some(shim) = extra.get("PATH").filter(|shim| !shim.is_empty()) {
        out.insert(
            "PATH".into(),
            match base_path {
                Some(base) if !base.is_empty() => format!("{shim}{separator}{base}"),
                _ => shim.clone(),
            },
        );
    }
    out
}

/// Resolves an image attachment to its stored file (`resolveAttachmentPath`, WP-08).
pub trait AttachmentResolver: Send + Sync {
    fn resolve(&self, attachment: &ChatAttachment) -> Option<PathBuf>;
}

/// The attachment store layout: `<attachmentsDir>/<id><ext>`.
#[derive(Debug, Clone)]
pub struct AttachmentsDir(pub PathBuf);

fn image_extension(mime_type: &str, file_name: &str) -> String {
    let by_mime = match mime_type.to_lowercase().as_str() {
        "image/avif" => Some(".avif"),
        "image/bmp" => Some(".bmp"),
        "image/gif" => Some(".gif"),
        "image/heic" => Some(".heic"),
        "image/heif" => Some(".heif"),
        "image/jpeg" | "image/jpg" => Some(".jpg"),
        "image/png" => Some(".png"),
        "image/svg+xml" => Some(".svg"),
        "image/tiff" => Some(".tiff"),
        "image/webp" => Some(".webp"),
        "image/x-icon" | "image/vnd.microsoft.icon" => Some(".ico"),
        _ => None,
    };
    if let Some(extension) = by_mime {
        return extension.to_owned();
    }
    const SAFE: &[&str] = &[
        ".avif", ".bmp", ".gif", ".heic", ".heif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".tiff", ".webp",
    ];
    let name = file_name.trim();
    if let Some(index) = name.rfind('.') {
        let extension = &name[index + 1..];
        if (1..=8).contains(&extension.len()) && extension.chars().all(|c| c.is_ascii_alphanumeric()) {
            let dotted = format!(".{}", extension.to_lowercase());
            if SAFE.contains(&dotted.as_str()) {
                return dotted;
            }
        }
    }
    ".bin".to_owned()
}

impl AttachmentResolver for AttachmentsDir {
    fn resolve(&self, attachment: &ChatAttachment) -> Option<PathBuf> {
        let ChatAttachment::ChatImageAttachment(image) = attachment else {
            return None;
        };
        let relative = format!("{}{}", image.id, image_extension(&image.mime_type, &image.name));
        let relative = relative.trim_start_matches(['/', '\\']);
        if relative.is_empty() || relative.starts_with("..") || relative.contains('\0') || relative.contains("/..") || relative.contains('/') {
            return None;
        }
        let root = zc_core::paths::resolve_path(&self.0);
        let path = root.join(relative);
        path.starts_with(&root).then_some(path)
    }
}

/// `CodexAdapterLiveOptions`.
#[derive(Clone, Default)]
pub struct CodexAdapterOptions {
    pub instance_id: Option<ProviderInstanceId>,
    pub environment: Option<Environment>,
    pub models: Option<ModelsSource>,
    pub make_runtime: Option<RuntimeFactory>,
    pub resolve_runtime: Option<ResolveRuntime>,
    pub on_managed_connection_revoked: Option<OnRevoked>,
    pub native_event_sink: Option<Arc<dyn NativeEventSink>>,
    pub mcp_sessions: Option<Arc<dyn McpSessionLookup>>,
    pub attachments: Option<Arc<dyn AttachmentResolver>>,
    /// The cwd of sessions started without one (`process.cwd()`).
    pub default_cwd: Option<String>,
}

struct SessionContext {
    runtime: Arc<dyn CodexRuntime>,
    mapper: Arc<Mutex<CodexEventMapper>>,
    start_input: ProviderSessionStartInput,
    runtime_revision: Option<String>,
}

struct Inner {
    config: CodexSettings,
    options: CodexAdapterOptions,
    events: PubSub<ProviderRuntimeEvent>,
    sessions: tokio::sync::Mutex<HashMap<ThreadId, SessionContext>>,
}

/// The adapter of one Codex instance (`makeCodexAdapter(codexConfig, options)`).
#[derive(Clone)]
pub struct CodexAdapter {
    inner: Arc<Inner>,
}

/// The environment of the server process (`process.env`).
pub fn process_environment() -> Environment {
    std::env::vars().collect()
}

/// `mapCodexRuntimeError`.
fn map_runtime_error(thread_id: &ThreadId, method: &str, error: CodexSessionRuntimeError) -> AdapterError {
    match &error {
        CodexSessionRuntimeError::AppServer(CodexAppServerError::ProcessExited { .. } | CodexAppServerError::Transport { .. }) => AdapterError::SessionClosed {
            provider: provider(),
            thread_id: thread_id.as_str().to_owned(),
        },
        CodexSessionRuntimeError::ThreadIdMissing { .. } => AdapterError::SessionNotFound {
            provider: provider(),
            thread_id: thread_id.as_str().to_owned(),
        },
        _ => AdapterError::Request {
            provider: provider(),
            method: method.to_owned(),
            detail: error.to_string(),
        },
    }
}

fn default_factory() -> RuntimeFactory {
    Arc::new(|options| {
        Box::pin(async move {
            let runtime = CodexSessionRuntime::spawn(options)?;
            Ok(runtime as Arc<dyn CodexRuntime>)
        })
    })
}

impl CodexAdapter {
    pub fn new(config: CodexSettings, options: CodexAdapterOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                options,
                events: PubSub::new(),
                sessions: tokio::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    fn bound_instance_id(&self) -> ProviderInstanceId {
        self.inner
            .options
            .instance_id
            .clone()
            .unwrap_or_else(|| ProviderInstanceId::new(crate::DRIVER_KIND))
    }

    fn selection_for_instance<'a>(&self, selection: Option<&'a ModelSelection>) -> Option<&'a ModelSelection> {
        selection.filter(|selection| selection.instance_id == self.bound_instance_id())
    }

    /// The runtime options a session starts with (exposed for the argv/env oracle tests).
    pub fn runtime_options(&self, input: &ProviderSessionStartInput, resolved: Option<&CodexEffectiveRuntime>) -> CodexSessionRuntimeOptions {
        let options = &self.inner.options;
        let config = resolved.map_or(&self.inner.config, |resolved| &resolved.config);
        let environment = resolved.map(|resolved| resolved.environment.clone()).or_else(|| options.environment.clone());
        let selection = self.selection_for_instance(input.model_selection.as_ref());
        let service_tier = if resolved.is_none() {
            codex_service_tier_option_value(selection)
        } else {
            None
        };
        let launch_environment = environment.clone().unwrap_or_else(process_environment);
        let mut runtime = CodexSessionRuntimeOptions::new(
            input.thread_id.clone(),
            config.binary_path.clone(),
            input
                .cwd
                .clone()
                .or_else(|| options.default_cwd.clone())
                .unwrap_or_else(|| std::env::current_dir().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default()),
            input.runtime_mode,
        );
        runtime.provider_instance_id = Some(self.bound_instance_id());
        runtime.models = options.models.clone();
        runtime.launch_args = Some(resolve_codex_launch_args(Some(&config.launch_args), &launch_environment));
        runtime.environment = environment.clone();
        runtime.home_path = Some(config.home_path.clone()).filter(|home| !home.is_empty());
        runtime.resume_cursor = input
            .resume_cursor
            .clone()
            .filter(|cursor| cursor.get("threadId").is_some_and(Value::is_string));
        runtime.model = selection.map(|selection| selection.model.clone());
        runtime.service_tier = service_tier;
        if let Some(mcp) = options.mcp_sessions.as_ref().and_then(|lookup| lookup.read(&input.thread_id)) {
            let base = environment.unwrap_or_else(process_environment);
            let mut env = with_agent_device_environment(&base, &mcp);
            // `authorizationHeader.replace(/^Bearer\s+/, "")`
            let token = match mcp.authorization_header.strip_prefix("Bearer") {
                Some(rest) if rest.starts_with(char::is_whitespace) => rest.trim_start().to_owned(),
                _ => mcp.authorization_header.clone(),
            };
            env.insert("T3_MCP_BEARER_TOKEN".into(), token);
            runtime.environment = Some(env);
            runtime.app_server_args = Some(vec![
                "-c".into(),
                format!("mcp_servers.t3-code.url={}", mcp.endpoint),
                "-c".into(),
                "mcp_servers.t3-code.bearer_token_env_var=\"T3_MCP_BEARER_TOKEN\"".into(),
            ]);
            runtime.mcp_capabilities = Some(mcp.capabilities.clone());
        }
        runtime
    }

    /// Closes the runtime; its event consumer drains what was emitted while closing (the
    /// `session/closed` event), then ends with the channel.
    async fn stop_session_internal(&self, context: SessionContext) {
        context.runtime.close().await;
    }

    async fn resolve_runtime(&self, operation: &str) -> AdapterResult<Option<CodexEffectiveRuntime>> {
        let Some(resolve) = &self.inner.options.resolve_runtime else {
            return Ok(None);
        };
        resolve().await.map(Some).map_err(|error| AdapterError::Validation {
            provider: provider(),
            operation: operation.to_owned(),
            issue: error.detail,
        })
    }

    async fn start_session_inner(&self, input: ProviderSessionStartInput) -> AdapterResult<ProviderSession> {
        if let Some(requested) = &input.provider {
            if requested.as_str() != crate::DRIVER_KIND {
                return Err(AdapterError::Validation {
                    provider: provider(),
                    operation: "startSession".into(),
                    issue: format!("Expected provider '{}' but received '{requested}'.", crate::DRIVER_KIND),
                });
            }
        }
        let existing = self.inner.sessions.lock().await.remove(&input.thread_id);
        if let Some(existing) = existing {
            self.stop_session_internal(existing).await;
        }
        let resolved = self.resolve_runtime("startSession").await?;
        let runtime_options = self.runtime_options(&input, resolved.as_ref());
        let factory = self.inner.options.make_runtime.clone().unwrap_or_else(default_factory);
        let thread_id = input.thread_id.clone();
        let runtime = factory(runtime_options).await.map_err(|error| AdapterError::Process {
            provider: provider(),
            thread_id: thread_id.as_str().to_owned(),
            detail: error.to_string(),
        })?;
        let mapper = Arc::new(Mutex::new(CodexEventMapper::new(self.inner.options.resolve_runtime.is_some())));
        let consumer = self.spawn_consumer(&runtime, mapper.clone());
        let started = match runtime.start().await {
            Ok(started) => started,
            Err(error) => {
                runtime.close().await;
                consumer.abort();
                return Err(AdapterError::Process {
                    provider: provider(),
                    thread_id: thread_id.as_str().to_owned(),
                    detail: error.to_string(),
                });
            }
        };
        self.inner.sessions.lock().await.insert(
            thread_id,
            SessionContext {
                runtime,
                mapper,
                start_input: input,
                runtime_revision: resolved.map(|resolved| resolved.revision),
            },
        );
        Ok(started)
    }

    fn spawn_consumer(&self, runtime: &Arc<dyn CodexRuntime>, mapper: Arc<Mutex<CodexEventMapper>>) -> AbortHandle {
        let Some(mut events) = runtime.take_events() else {
            return tokio::spawn(async {}).abort_handle();
        };
        let inner = self.inner.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if let Some(sink) = &inner.options.native_event_sink {
                    sink.write(&event);
                }
                let mapped = mapper.lock().expect("codex mapper lock").process(&event);
                if mapped.revoke {
                    if let Some(on_revoked) = &inner.options.on_managed_connection_revoked {
                        on_revoked().await;
                    }
                }
                if mapped.events.is_empty() {
                    tracing::debug!(method = %event.method, thread_id = %event.thread_id.as_str(), "ignoring unhandled Codex provider event");
                    continue;
                }
                for value in mapped.events {
                    match to_runtime_event(value.clone()) {
                        Ok(runtime_event) => {
                            inner.events.publish(runtime_event);
                        }
                        Err(error) => tracing::error!(%error, event = %value, "a mapped Codex runtime event does not decode"),
                    }
                }
            }
        })
        .abort_handle()
    }

    async fn require_runtime(&self, thread_id: &ThreadId) -> AdapterResult<Arc<dyn CodexRuntime>> {
        self.inner
            .sessions
            .lock()
            .await
            .get(thread_id)
            .map(|context| context.runtime.clone())
            .ok_or_else(|| AdapterError::SessionNotFound {
                provider: provider(),
                thread_id: thread_id.as_str().to_owned(),
            })
    }

    fn snapshot(thread_id: &ThreadId, snapshot: CodexThreadSnapshot) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: thread_id.clone(),
            turns: snapshot
                .turns
                .into_iter()
                .map(|turn| ThreadTurnSnapshot {
                    id: TurnId::new(turn.id),
                    items: turn.items,
                })
                .collect(),
        }
    }

    /// The canonical events of every session, as JSON in the TS shape (for logs and tests).
    pub fn subscribe(&self) -> zc_core::Subscription<ProviderRuntimeEvent> {
        self.inner.events.subscribe()
    }
}

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    fn provider(&self) -> ProviderDriverKind {
        ProviderDriverKind::new(crate::DRIVER_KIND)
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            session_model_switch: SessionModelSwitch::InSession,
            promptless_turn_continuation: true,
            supports_conversation_rollback: true,
        }
    }

    fn compaction(&self) -> Option<Compaction> {
        Some(Compaction::Native)
    }

    async fn start_session(&self, input: ProviderSessionStartInput) -> AdapterResult<ProviderSession> {
        self.start_session_inner(input).await
    }

    async fn send_turn(&self, input: ProviderSendTurnInput) -> AdapterResult<ProviderTurnStartResult> {
        // Codex ingests images only, by path (the CLI reads the file); other files reach the
        // agent through the path line ProviderService puts in the prompt.
        let mut attachments = Vec::new();
        for attachment in input.attachments.iter().flatten() {
            let ChatAttachment::ChatImageAttachment(image) = attachment else { continue };
            let path = self.inner.options.attachments.as_ref().and_then(|resolver| resolver.resolve(attachment));
            let Some(path) = path else {
                return Err(AdapterError::Request {
                    provider: provider(),
                    method: "turn/start".into(),
                    detail: format!("Invalid attachment id '{}'.", image.id),
                });
            };
            attachments.push(path.to_string_lossy().into_owned());
        }
        let mut runtime = self.require_runtime(&input.thread_id).await?;
        if self.inner.options.resolve_runtime.is_some() {
            let next = self.resolve_runtime("sendTurn").await?.expect("resolver present");
            let (current_revision, start_input) = {
                let sessions = self.inner.sessions.lock().await;
                let context = sessions.get(&input.thread_id);
                (
                    context.and_then(|context| context.runtime_revision.clone()),
                    context.map(|context| context.start_input.clone()),
                )
            };
            if Some(&next.revision) != current_revision.as_ref() {
                let previous = runtime.get_session().await;
                if let Some(mut start_input) = start_input {
                    if previous.resume_cursor.is_some() {
                        start_input.resume_cursor = previous.resume_cursor.clone();
                    }
                    self.start_session_inner(start_input).await?;
                }
                runtime = self.require_runtime(&input.thread_id).await?;
            }
        }
        let selection = self.selection_for_instance(input.model_selection.as_ref());
        let turn = SendTurnInput {
            input: input.input.clone(),
            attachments: (!attachments.is_empty()).then_some(attachments),
            model: selection.map(|selection| selection.model.clone()),
            service_tier: if self.inner.options.resolve_runtime.is_none() {
                codex_service_tier_option_value(selection)
            } else {
                None
            },
            effort: model_selection_string_option(selection, "reasoningEffort"),
            interaction_mode: input.interaction_mode,
        };
        runtime
            .send_turn(turn)
            .await
            .map_err(|error| map_runtime_error(&input.thread_id, "turn/start", error))
    }

    async fn start_compaction(&self, thread_id: &ThreadId, _model_selection: Option<ModelSelection>) -> AdapterResult<()> {
        let runtime = self.require_runtime(thread_id).await?;
        runtime
            .compact_thread()
            .await
            .map_err(|error| map_runtime_error(thread_id, "thread/compact/start", error))
    }

    async fn interrupt_turn(&self, thread_id: &ThreadId, turn_id: Option<&TurnId>) -> AdapterResult<()> {
        let runtime = self.require_runtime(thread_id).await?;
        runtime
            .interrupt_turn(turn_id.cloned())
            .await
            .map_err(|error| map_runtime_error(thread_id, "turn/interrupt", error))
    }

    async fn respond_to_request(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, decision: ProviderApprovalDecision) -> AdapterResult<()> {
        let runtime = self.require_runtime(thread_id).await?;
        runtime
            .respond_to_request(request_id, decision)
            .await
            .map_err(|error| map_runtime_error(thread_id, "item/requestApproval/decision", error))
    }

    async fn respond_to_user_input(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, answers: ProviderUserInputAnswers) -> AdapterResult<()> {
        let runtime = self.require_runtime(thread_id).await?;
        runtime
            .respond_to_user_input(request_id, answers)
            .await
            .map_err(|error| map_runtime_error(thread_id, "item/tool/requestUserInput", error))
    }

    async fn stop_session(&self, thread_id: &ThreadId) -> AdapterResult<()> {
        let context = self.inner.sessions.lock().await.remove(thread_id);
        if let Some(context) = context {
            self.stop_session_internal(context).await;
        }
        Ok(())
    }

    async fn list_sessions(&self) -> Vec<ProviderSession> {
        let runtimes: Vec<Arc<dyn CodexRuntime>> = self.inner.sessions.lock().await.values().map(|context| context.runtime.clone()).collect();
        let mut sessions = Vec::with_capacity(runtimes.len());
        for runtime in runtimes {
            sessions.push(runtime.get_session().await);
        }
        sessions
    }

    async fn has_session(&self, thread_id: &ThreadId) -> bool {
        self.inner.sessions.lock().await.contains_key(thread_id)
    }

    async fn read_thread(&self, thread_id: &ThreadId) -> AdapterResult<ThreadSnapshot> {
        let runtime = self.require_runtime(thread_id).await?;
        let snapshot = runtime
            .read_thread()
            .await
            .map_err(|error| map_runtime_error(thread_id, "thread/read", error))?;
        Ok(Self::snapshot(thread_id, snapshot))
    }

    async fn rollback_thread(&self, thread_id: &ThreadId, num_turns: u32) -> AdapterResult<ThreadSnapshot> {
        if num_turns < 1 {
            return Err(AdapterError::Validation {
                provider: provider(),
                operation: "rollbackThread".into(),
                issue: "numTurns must be an integer >= 1.".into(),
            });
        }
        let (runtime, mapper) = {
            let sessions = self.inner.sessions.lock().await;
            let context = sessions.get(thread_id).ok_or_else(|| AdapterError::SessionNotFound {
                provider: provider(),
                thread_id: thread_id.as_str().to_owned(),
            })?;
            (context.runtime.clone(), context.mapper.clone())
        };
        let snapshot = runtime
            .rollback_thread(num_turns as usize)
            .await
            .map_err(|error| map_runtime_error(thread_id, "thread/rollback", error))?;
        mapper.lock().expect("codex mapper lock").token_usage.reset();
        Ok(Self::snapshot(thread_id, snapshot))
    }

    async fn upload_feedback(&self, input: ProviderUploadFeedbackInput) -> Option<AdapterResult<ProviderUploadFeedbackResult>> {
        let result = async {
            let runtime = self.require_runtime(&input.thread_id).await?;
            let thread_id = runtime
                .upload_feedback(input.reason.clone())
                .await
                .map_err(|error| map_runtime_error(&input.thread_id, "feedback/upload", error))?;
            Ok(ProviderUploadFeedbackResult { feedback_id: thread_id })
        }
        .await;
        Some(result)
    }

    async fn stop_all(&self) -> AdapterResult<()> {
        let contexts: Vec<SessionContext> = self.inner.sessions.lock().await.drain().map(|(_, context)| context).collect();
        for context in contexts {
            self.stop_session_internal(context).await;
        }
        Ok(())
    }

    fn subscribe_events(&self) -> BoxStream<'static, ProviderRuntimeEvent> {
        self.inner.events.subscribe().boxed()
    }
}

#[cfg(test)]
mod tests;
