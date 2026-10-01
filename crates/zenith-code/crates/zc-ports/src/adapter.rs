//! The provider adapter: what each driver (Claude, Codex, ACP agents, OpenCode) implements,
//! and what the provider service routes to once it knows the target instance. Port of
//! code/apps/server/src/provider/Services/ProviderAdapter.ts and the adapter errors of
//! provider/Errors.ts. Drivers focus on their provider's protocol; cross-provider concerns
//! (routing, session directory, logging) stay in the provider service.

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Serialize;
use zc_contracts::{
    ApprovalRequestId, ModelSelection, ProviderApprovalDecision, ProviderDriverKind, ProviderRuntimeEvent, ProviderSendTurnInput, ProviderSession,
    ProviderSessionStartInput, ProviderTurnStartResult, ProviderUploadFeedbackInput, ProviderUploadFeedbackResult, ProviderUserInputAnswers, ThreadId, TurnId,
};

/// Whether changing the model on an existing session works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionModelSwitch {
    InSession,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterCapabilities {
    pub session_model_switch: SessionModelSwitch,
    /// Starts a resumed turn with no synthetic user prompt; false means the adapter needs
    /// an explicit continuation instruction.
    pub promptless_turn_continuation: bool,
    /// False when native conversation history cannot be rewound.
    pub supports_conversation_rollback: bool,
}

/// How the provider service runs manual context compaction for an adapter: natively
/// (`ProviderAdapter::start_compaction`, which must end with a compacted thread state), or
/// by sending a slash command as a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compaction {
    Native,
    SlashCommand(&'static str),
}

#[derive(Clone, Debug, Serialize)]
pub struct ThreadTurnSnapshot {
    pub id: TurnId,
    pub items: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSnapshot {
    pub thread_id: ThreadId,
    pub turns: Vec<ThreadTurnSnapshot>,
}

/// What an adapter call can fail with (`ProviderAdapterError` in TS): the `_tag` and fields
/// cross into the orchestration's activities as they did there.
#[derive(Clone, Debug, thiserror::Error, Serialize)]
#[serde(tag = "_tag")]
pub enum AdapterError {
    #[error("Provider adapter validation failed ({provider}) in {operation}: {issue}")]
    #[serde(rename = "ProviderAdapterValidationError")]
    Validation { provider: String, operation: String, issue: String },
    #[error("Unknown {provider} adapter thread: {thread_id}")]
    #[serde(rename = "ProviderAdapterSessionNotFoundError", rename_all = "camelCase")]
    SessionNotFound { provider: String, thread_id: String },
    #[error("{provider} adapter thread is closed: {thread_id}")]
    #[serde(rename = "ProviderAdapterSessionClosedError", rename_all = "camelCase")]
    SessionClosed { provider: String, thread_id: String },
    #[error("Provider adapter request failed ({provider}) for {method}: {detail}")]
    #[serde(rename = "ProviderAdapterRequestError")]
    Request { provider: String, method: String, detail: String },
    #[error("Provider adapter process error ({provider}) for thread {thread_id}: {detail}")]
    #[serde(rename = "ProviderAdapterProcessError", rename_all = "camelCase")]
    Process { provider: String, thread_id: String, detail: String },
}

pub type AdapterResult<T> = Result<T, AdapterError>;

/// One provider's runtime: sessions, turns, approvals, user input, its event stream.
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    /// The driver kind it implements.
    fn provider(&self) -> ProviderDriverKind;
    fn capabilities(&self) -> AdapterCapabilities;
    /// None when manual context compaction isn't supported.
    fn compaction(&self) -> Option<Compaction> {
        None
    }

    async fn start_session(&self, input: ProviderSessionStartInput) -> AdapterResult<ProviderSession>;
    async fn send_turn(&self, input: ProviderSendTurnInput) -> AdapterResult<ProviderTurnStartResult>;
    /// Native compaction (only called when `compaction()` is `Native`).
    async fn start_compaction(&self, thread_id: &ThreadId, model_selection: Option<ModelSelection>) -> AdapterResult<()> {
        let _ = model_selection;
        Err(AdapterError::Validation {
            provider: self.provider().to_string(),
            operation: "compaction".into(),
            issue: format!("not supported for {thread_id}"),
        })
    }
    async fn interrupt_turn(&self, thread_id: &ThreadId, turn_id: Option<&TurnId>) -> AdapterResult<()>;
    async fn respond_to_request(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, decision: ProviderApprovalDecision) -> AdapterResult<()>;
    async fn respond_to_user_input(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, answers: ProviderUserInputAnswers) -> AdapterResult<()>;
    async fn stop_session(&self, thread_id: &ThreadId) -> AdapterResult<()>;
    async fn list_sessions(&self) -> Vec<ProviderSession>;
    async fn has_session(&self, thread_id: &ThreadId) -> bool;
    async fn read_thread(&self, thread_id: &ThreadId) -> AdapterResult<ThreadSnapshot>;
    async fn rollback_thread(&self, thread_id: &ThreadId, num_turns: u32) -> AdapterResult<ThreadSnapshot>;
    /// None when the adapter can't upload feedback.
    async fn upload_feedback(&self, input: ProviderUploadFeedbackInput) -> Option<AdapterResult<ProviderUploadFeedbackResult>> {
        let _ = input;
        None
    }
    async fn stop_all(&self) -> AdapterResult<()>;
    /// The canonical runtime events it emits; every subscriber gets every event.
    fn subscribe_events(&self) -> BoxStream<'static, ProviderRuntimeEvent>;
}
