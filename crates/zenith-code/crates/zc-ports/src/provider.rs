//! Provider ports (`apps/server/src/provider/Services/ProviderService.ts`, `ProviderAdapter.ts`,
//! `ProviderAdapterRegistry.ts`, `ProviderRegistry.ts`, `ProviderAuthService.ts`).
//!
//! Implemented by zc-providers; consumed by the orchestration reactors (`ProviderCommandReactor`,
//! `ProviderRuntimeIngestion`, `CheckpointReactor`, `ThreadDeletionReactor`) and startup.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::contracts::{
    MessageId, ModelSelection, ProviderDriverKind, ProviderInstanceId, ProviderInterruptTurnInput, ProviderRespondToRequestInput,
    ProviderRespondToUserInputInput, ProviderRuntimeEvent, ProviderSendTurnInput, ProviderServiceError, ProviderSession, ProviderSessionStartInput,
    ProviderSetupError, ProviderStopSessionInput, ProviderTurnStartResult, ProviderUploadFeedbackInput, ProviderUploadFeedbackResult, ServerProvider, ThreadId,
};
use crate::EventStream;

/// `ProviderSessionModelSwitchMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionModelSwitchMode {
    InSession,
    Unsupported,
}

/// `ProviderAdapterCapabilities` (server-internal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAdapterCapabilities {
    /// Whether changing the model on an existing session is supported.
    pub session_model_switch: SessionModelSwitchMode,
    /// Starts a resumed turn with no synthetic user prompt. `None` means the adapter needs an
    /// explicit continuation instruction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promptless_turn_continuation: Option<bool>,
    /// `Some(false)` when native conversation history cannot be rewound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_conversation_rollback: Option<bool>,
}

/// `ProviderContinuationIdentity` (`ProviderDriver.ts`): sessions can only be continued by an
/// instance with the same identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderContinuationIdentity {
    pub driver_kind: ProviderDriverKind,
    pub continuation_key: String,
}

impl ProviderContinuationIdentity {
    /// `defaultProviderContinuationIdentity`: `<driverKind>:instance:<instanceId>`.
    pub fn default_for(driver_kind: &ProviderDriverKind, instance_id: &ProviderInstanceId) -> Self {
        Self {
            driver_kind: driver_kind.clone(),
            continuation_key: format!("{driver_kind}:instance:{instance_id}"),
        }
    }
}

/// `ProviderInstanceRoutingInfo` (`ProviderAdapterRegistry.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInstanceRoutingInfo {
    pub instance_id: ProviderInstanceId,
    pub driver_kind: ProviderDriverKind,
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub continuation_identity: ProviderContinuationIdentity,
}

/// `ProviderService`: the cross-provider facade. Routes session-scoped calls to the adapter
/// that owns the thread and fans every adapter's events into one stream.
#[async_trait]
pub trait ProviderService: Send + Sync {
    /// `startSession(threadId, input)`.
    async fn start_session(&self, thread_id: &ThreadId, input: ProviderSessionStartInput) -> Result<ProviderSession, ProviderServiceError>;

    /// `sendTurn(input)`. A send during a running turn is a steer for providers that support it.
    async fn send_turn(&self, input: ProviderSendTurnInput) -> Result<ProviderTurnStartResult, ProviderServiceError>;

    /// `compactThread(threadId, modelSelection?, requestId?)`: native compaction or the
    /// adapter's slash command (`/compact`).
    async fn compact_thread(
        &self,
        thread_id: &ThreadId,
        model_selection: Option<ModelSelection>,
        request_id: Option<MessageId>,
    ) -> Result<(), ProviderServiceError>;

    /// `interruptTurn(input)`.
    async fn interrupt_turn(&self, input: ProviderInterruptTurnInput) -> Result<(), ProviderServiceError>;

    /// `respondToRequest(input)`: an approval decision.
    async fn respond_to_request(&self, input: ProviderRespondToRequestInput) -> Result<(), ProviderServiceError>;

    /// `respondToUserInput(input)`: answers to a structured question.
    async fn respond_to_user_input(&self, input: ProviderRespondToUserInputInput) -> Result<(), ProviderServiceError>;

    /// `stopSession(input)`.
    async fn stop_session(&self, input: ProviderStopSessionInput) -> Result<(), ProviderServiceError>;

    /// `listSessions()`: live sessions of every adapter.
    async fn list_sessions(&self) -> Vec<ProviderSession>;

    /// `getCapabilities(instanceId)`.
    async fn get_capabilities(&self, instance_id: &ProviderInstanceId) -> Result<ProviderAdapterCapabilities, ProviderServiceError>;

    /// `getInstanceInfo(instanceId)`.
    async fn get_instance_info(&self, instance_id: &ProviderInstanceId) -> Result<ProviderInstanceRoutingInfo, ProviderServiceError>;

    /// `assertConversationRollbackSupported(threadId)`: reject an unsupported rewind before any
    /// file changes, without resuming the session.
    async fn assert_conversation_rollback_supported(&self, thread_id: &ThreadId) -> Result<(), ProviderServiceError>;

    /// `rollbackConversation({threadId, numTurns})`.
    async fn rollback_conversation(&self, thread_id: &ThreadId, num_turns: u32) -> Result<(), ProviderServiceError>;

    /// `uploadFeedback(input)`.
    async fn upload_feedback(&self, input: ProviderUploadFeedbackInput) -> Result<ProviderUploadFeedbackResult, ProviderServiceError>;

    /// `streamEvents`: canonical runtime events of every session. The subscription exists when
    /// this returns (events published afterwards are never missed) and is unbounded.
    fn subscribe_events(&self) -> EventStream<ProviderRuntimeEvent>;
}

/// The part of `ProviderRegistry` the orchestration reactors read: the status snapshots that
/// also feed `subscribeServerConfig.providerStatuses`.
#[async_trait]
pub trait ProviderStatusReads: Send + Sync {
    /// `getProviders`: one `ServerProvider` per configured instance.
    async fn get_providers(&self) -> Vec<ServerProvider>;
}

/// The part of `ProviderAuthService` the provider command reactor needs.
#[async_trait]
pub trait ProviderAuthCommands: Send + Sync {
    /// `tryHandlePromptCommand({instanceId, text, hasAttachments})`: true when the prompt was an
    /// auth command (e.g. `/login`) that the auth service handled instead of the provider.
    async fn try_handle_prompt_command(&self, instance_id: &ProviderInstanceId, text: &str, has_attachments: bool) -> Result<bool, ProviderSetupError>;
}
