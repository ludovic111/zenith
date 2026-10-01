//! The `zc_ports` faces of the provider core: [`zc_ports::ProviderService`] over
//! [`ProviderServiceImpl`] and [`zc_ports::ProviderStatusReads`] over [`ProviderRegistry`].
//!
//! The ports still carry the `zc_ports::contracts` placeholders (JSON newtypes); values cross by
//! serde, which is lossless since both sides model the same wire JSON. A payload that does not
//! decode is a `ProviderValidationError`, as the TS schema decode would be.

use async_trait::async_trait;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_ports::contracts as ports;
use zc_ports::provider::{ProviderAdapterCapabilities, ProviderContinuationIdentity, ProviderInstanceRoutingInfo, SessionModelSwitchMode};
use zc_ports::{EventStream, TaggedError};

use crate::errors::ProviderServiceError;
use crate::registry::ProviderRegistry;
use crate::service::ProviderServiceImpl;

fn decode<T: DeserializeOwned>(operation: &str, value: Value) -> Result<T, TaggedError> {
    serde_json::from_value(value).map_err(|error| ProviderServiceError::validation(operation, error.to_string()).to_tagged())
}

fn encode<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn tagged(error: ProviderServiceError) -> TaggedError {
    error.to_tagged()
}

fn routing_info(info: crate::adapter_registry::RoutingInfo) -> ProviderInstanceRoutingInfo {
    ProviderInstanceRoutingInfo {
        instance_id: ports::ProviderInstanceId::new(info.instance_id.as_str()),
        driver_kind: ports::ProviderDriverKind::new(info.driver_kind.as_str()),
        display_name: info.display_name,
        accent_color: info.accent_color,
        enabled: info.enabled,
        continuation_identity: ProviderContinuationIdentity {
            driver_kind: ports::ProviderDriverKind::new(info.continuation_identity.driver_kind.as_str()),
            continuation_key: info.continuation_identity.continuation_key,
        },
    }
}

#[async_trait]
impl zc_ports::ProviderService for ProviderServiceImpl {
    async fn start_session(&self, thread_id: &ports::ThreadId, input: ports::ProviderSessionStartInput) -> Result<ports::ProviderSession, TaggedError> {
        let input = decode("ProviderService.startSession", input.0)?;
        let session = self
            .start_session(&zc_contracts::ThreadId::new(thread_id.as_str()), input)
            .await
            .map_err(tagged)?;
        Ok(ports::ProviderSession(encode(&session)))
    }

    async fn send_turn(&self, input: ports::ProviderSendTurnInput) -> Result<ports::ProviderTurnStartResult, TaggedError> {
        let input = decode("ProviderService.sendTurn", input.0)?;
        let turn = self.send_turn(input).await.map_err(tagged)?;
        Ok(ports::ProviderTurnStartResult(encode(&turn)))
    }

    async fn compact_thread(
        &self,
        thread_id: &ports::ThreadId,
        model_selection: Option<ports::ModelSelection>,
        request_id: Option<ports::MessageId>,
    ) -> Result<(), TaggedError> {
        let model_selection = model_selection
            .map(|selection| decode("ProviderService.compactThread", selection.0))
            .transpose()?;
        self.compact_thread(
            &zc_contracts::ThreadId::new(thread_id.as_str()),
            model_selection,
            request_id.map(|id| zc_contracts::MessageId::new(id.as_str())),
        )
        .await
        .map_err(tagged)
    }

    async fn interrupt_turn(&self, input: ports::ProviderInterruptTurnInput) -> Result<(), TaggedError> {
        let input = decode("ProviderService.interruptTurn", input.0)?;
        self.interrupt_turn(input).await.map_err(tagged)
    }

    async fn respond_to_request(&self, input: ports::ProviderRespondToRequestInput) -> Result<(), TaggedError> {
        let input = decode("ProviderService.respondToRequest", input.0)?;
        self.respond_to_request(input).await.map_err(tagged)
    }

    async fn respond_to_user_input(&self, input: ports::ProviderRespondToUserInputInput) -> Result<(), TaggedError> {
        let input = decode("ProviderService.respondToUserInput", input.0)?;
        self.respond_to_user_input(input).await.map_err(tagged)
    }

    async fn stop_session(&self, input: ports::ProviderStopSessionInput) -> Result<(), TaggedError> {
        let input: zc_contracts::ProviderStopSessionInput = decode("ProviderService.stopSession", input.0)?;
        self.stop_session(&input.thread_id).await.map_err(tagged)
    }

    async fn list_sessions(&self) -> Vec<ports::ProviderSession> {
        self.list_sessions()
            .await
            .iter()
            .map(|session| ports::ProviderSession(encode(session)))
            .collect()
    }

    async fn get_capabilities(&self, instance_id: &ports::ProviderInstanceId) -> Result<ProviderAdapterCapabilities, TaggedError> {
        let capabilities = self
            .get_capabilities(&zc_contracts::ProviderInstanceId::new(instance_id.as_str()))
            .map_err(tagged)?;
        Ok(ProviderAdapterCapabilities {
            session_model_switch: match capabilities.session_model_switch {
                zc_ports::adapter::SessionModelSwitch::InSession => SessionModelSwitchMode::InSession,
                zc_ports::adapter::SessionModelSwitch::Unsupported => SessionModelSwitchMode::Unsupported,
            },
            promptless_turn_continuation: Some(capabilities.promptless_turn_continuation),
            supports_conversation_rollback: Some(capabilities.supports_conversation_rollback),
        })
    }

    async fn get_instance_info(&self, instance_id: &ports::ProviderInstanceId) -> Result<ProviderInstanceRoutingInfo, TaggedError> {
        self.get_instance_info(&zc_contracts::ProviderInstanceId::new(instance_id.as_str()))
            .map(routing_info)
            .map_err(tagged)
    }

    async fn assert_conversation_rollback_supported(&self, thread_id: &ports::ThreadId) -> Result<(), TaggedError> {
        self.assert_conversation_rollback_supported(&zc_contracts::ThreadId::new(thread_id.as_str()))
            .await
            .map_err(tagged)
    }

    async fn rollback_conversation(&self, thread_id: &ports::ThreadId, num_turns: u32) -> Result<(), TaggedError> {
        self.rollback_conversation(&zc_contracts::ThreadId::new(thread_id.as_str()), num_turns)
            .await
            .map_err(tagged)
    }

    async fn upload_feedback(&self, input: ports::ProviderUploadFeedbackInput) -> Result<ports::ProviderUploadFeedbackResult, TaggedError> {
        let input = decode("ProviderService.uploadFeedback", input.0)?;
        let result = self.upload_feedback(input).await.map_err(tagged)?;
        Ok(ports::ProviderUploadFeedbackResult(encode(&result)))
    }

    fn subscribe_events(&self) -> EventStream<ports::ProviderRuntimeEvent> {
        self.subscribe_events().map(|event| ports::ProviderRuntimeEvent(encode(&event))).boxed()
    }
}

#[async_trait]
impl zc_ports::ProviderStatusReads for ProviderRegistry {
    async fn get_providers(&self) -> Vec<ports::ServerProvider> {
        self.get_providers().iter().map(|provider| ports::ServerProvider(encode(provider))).collect()
    }
}

/// `subscribeServerConfig`'s `providerStatuses` source: the full list after each change.
pub fn provider_status_changes(registry: &ProviderRegistry) -> EventStream<Vec<ports::ServerProvider>> {
    registry
        .subscribe_changes()
        .map(|providers| providers.iter().map(|provider| ports::ServerProvider(encode(provider))).collect())
        .boxed()
}
