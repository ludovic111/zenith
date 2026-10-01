//! Header access for [`ProviderRuntimeEvent`]: all 49 members share the same envelope
//! (`eventId`, `provider`, `providerInstanceId`, `threadId`, `createdAt`, `turnId`, `itemId`,
//! `requestId`, …), which TS reads with `event.threadId` and Rust reads through these helpers.

use zc_contracts::{EventId, ProviderDriverKind, ProviderInstanceId, ProviderRuntimeEvent, RuntimeRequestId, ThreadId, TurnId};

macro_rules! for_each_member {
    ($event:expr, $inner:ident => $body:expr) => {
        match $event {
            ProviderRuntimeEvent::SessionStarted($inner) => $body,
            ProviderRuntimeEvent::SessionConfigured($inner) => $body,
            ProviderRuntimeEvent::SessionStateChanged($inner) => $body,
            ProviderRuntimeEvent::SessionExited($inner) => $body,
            ProviderRuntimeEvent::ThreadStarted($inner) => $body,
            ProviderRuntimeEvent::ThreadStateChanged($inner) => $body,
            ProviderRuntimeEvent::ThreadMetadataUpdated($inner) => $body,
            ProviderRuntimeEvent::ThreadTokenUsageUpdated($inner) => $body,
            ProviderRuntimeEvent::ThreadRealtimeStarted($inner) => $body,
            ProviderRuntimeEvent::ThreadRealtimeItemAdded($inner) => $body,
            ProviderRuntimeEvent::ThreadRealtimeAudioDelta($inner) => $body,
            ProviderRuntimeEvent::ThreadRealtimeError($inner) => $body,
            ProviderRuntimeEvent::ThreadRealtimeClosed($inner) => $body,
            ProviderRuntimeEvent::TurnStarted($inner) => $body,
            ProviderRuntimeEvent::TurnCompleted($inner) => $body,
            ProviderRuntimeEvent::TurnAborted($inner) => $body,
            ProviderRuntimeEvent::TurnPlanUpdated($inner) => $body,
            ProviderRuntimeEvent::TurnProposedDelta($inner) => $body,
            ProviderRuntimeEvent::TurnProposedCompleted($inner) => $body,
            ProviderRuntimeEvent::TurnDiffUpdated($inner) => $body,
            ProviderRuntimeEvent::ItemStarted($inner) => $body,
            ProviderRuntimeEvent::ItemUpdated($inner) => $body,
            ProviderRuntimeEvent::ItemCompleted($inner) => $body,
            ProviderRuntimeEvent::ContentDelta($inner) => $body,
            ProviderRuntimeEvent::RequestOpened($inner) => $body,
            ProviderRuntimeEvent::RequestResolved($inner) => $body,
            ProviderRuntimeEvent::UserInputRequested($inner) => $body,
            ProviderRuntimeEvent::UserInputResolved($inner) => $body,
            ProviderRuntimeEvent::TaskStarted($inner) => $body,
            ProviderRuntimeEvent::TaskProgress($inner) => $body,
            ProviderRuntimeEvent::TaskUpdated($inner) => $body,
            ProviderRuntimeEvent::TaskCompleted($inner) => $body,
            ProviderRuntimeEvent::HookStarted($inner) => $body,
            ProviderRuntimeEvent::HookProgress($inner) => $body,
            ProviderRuntimeEvent::HookCompleted($inner) => $body,
            ProviderRuntimeEvent::ToolProgress($inner) => $body,
            ProviderRuntimeEvent::ToolSummary($inner) => $body,
            ProviderRuntimeEvent::AuthStatus($inner) => $body,
            ProviderRuntimeEvent::AccountUpdated($inner) => $body,
            ProviderRuntimeEvent::AccountRateLimitsUpdated($inner) => $body,
            ProviderRuntimeEvent::McpStatusUpdated($inner) => $body,
            ProviderRuntimeEvent::McpOauthCompleted($inner) => $body,
            ProviderRuntimeEvent::ModelRerouted($inner) => $body,
            ProviderRuntimeEvent::ConfigWarning($inner) => $body,
            ProviderRuntimeEvent::DeprecationNotice($inner) => $body,
            ProviderRuntimeEvent::FilesPersisted($inner) => $body,
            ProviderRuntimeEvent::ToolDenied($inner) => $body,
            ProviderRuntimeEvent::RuntimeWarning($inner) => $body,
            ProviderRuntimeEvent::RuntimeError($inner) => $body,
        }
    };
}

/// The `type` discriminator.
pub fn event_type(event: &ProviderRuntimeEvent) -> &'static str {
    use ProviderRuntimeEvent as E;
    match event {
        E::SessionStarted(_) => "session.started",
        E::SessionConfigured(_) => "session.configured",
        E::SessionStateChanged(_) => "session.state.changed",
        E::SessionExited(_) => "session.exited",
        E::ThreadStarted(_) => "thread.started",
        E::ThreadStateChanged(_) => "thread.state.changed",
        E::ThreadMetadataUpdated(_) => "thread.metadata.updated",
        E::ThreadTokenUsageUpdated(_) => "thread.token-usage.updated",
        E::ThreadRealtimeStarted(_) => "thread.realtime.started",
        E::ThreadRealtimeItemAdded(_) => "thread.realtime.item-added",
        E::ThreadRealtimeAudioDelta(_) => "thread.realtime.audio.delta",
        E::ThreadRealtimeError(_) => "thread.realtime.error",
        E::ThreadRealtimeClosed(_) => "thread.realtime.closed",
        E::TurnStarted(_) => "turn.started",
        E::TurnCompleted(_) => "turn.completed",
        E::TurnAborted(_) => "turn.aborted",
        E::TurnPlanUpdated(_) => "turn.plan.updated",
        E::TurnProposedDelta(_) => "turn.proposed.delta",
        E::TurnProposedCompleted(_) => "turn.proposed.completed",
        E::TurnDiffUpdated(_) => "turn.diff.updated",
        E::ItemStarted(_) => "item.started",
        E::ItemUpdated(_) => "item.updated",
        E::ItemCompleted(_) => "item.completed",
        E::ContentDelta(_) => "content.delta",
        E::RequestOpened(_) => "request.opened",
        E::RequestResolved(_) => "request.resolved",
        E::UserInputRequested(_) => "user-input.requested",
        E::UserInputResolved(_) => "user-input.resolved",
        E::TaskStarted(_) => "task.started",
        E::TaskProgress(_) => "task.progress",
        E::TaskUpdated(_) => "task.updated",
        E::TaskCompleted(_) => "task.completed",
        E::HookStarted(_) => "hook.started",
        E::HookProgress(_) => "hook.progress",
        E::HookCompleted(_) => "hook.completed",
        E::ToolProgress(_) => "tool.progress",
        E::ToolSummary(_) => "tool.summary",
        E::AuthStatus(_) => "auth.status",
        E::AccountUpdated(_) => "account.updated",
        E::AccountRateLimitsUpdated(_) => "account.rate-limits.updated",
        E::McpStatusUpdated(_) => "mcp.status.updated",
        E::McpOauthCompleted(_) => "mcp.oauth.completed",
        E::ModelRerouted(_) => "model.rerouted",
        E::ConfigWarning(_) => "config.warning",
        E::DeprecationNotice(_) => "deprecation.notice",
        E::FilesPersisted(_) => "files.persisted",
        E::ToolDenied(_) => "tool.denied",
        E::RuntimeWarning(_) => "runtime.warning",
        E::RuntimeError(_) => "runtime.error",
    }
}

pub fn event_id(event: &ProviderRuntimeEvent) -> &EventId {
    for_each_member!(event, inner => &inner.event_id)
}

pub fn provider(event: &ProviderRuntimeEvent) -> &ProviderDriverKind {
    for_each_member!(event, inner => &inner.provider)
}

pub fn provider_instance_id(event: &ProviderRuntimeEvent) -> Option<&ProviderInstanceId> {
    for_each_member!(event, inner => inner.provider_instance_id.as_ref())
}

pub fn set_provider_instance_id(event: &mut ProviderRuntimeEvent, instance_id: ProviderInstanceId) {
    for_each_member!(event, inner => inner.provider_instance_id = Some(instance_id))
}

pub fn thread_id(event: &ProviderRuntimeEvent) -> &ThreadId {
    for_each_member!(event, inner => &inner.thread_id)
}

pub fn turn_id(event: &ProviderRuntimeEvent) -> Option<&TurnId> {
    for_each_member!(event, inner => inner.turn_id.as_ref())
}

pub fn created_at(event: &ProviderRuntimeEvent) -> &str {
    for_each_member!(event, inner => &inner.created_at)
}

pub fn set_request_id(event: &mut ProviderRuntimeEvent, request_id: RuntimeRequestId) {
    for_each_member!(event, inner => inner.request_id = Some(request_id))
}

/// `thread.state.changed` with `state: "compacted"`.
pub fn is_compacted(event: &ProviderRuntimeEvent) -> bool {
    matches!(
        event,
        ProviderRuntimeEvent::ThreadStateChanged(inner)
            if inner.payload.state == zc_contracts::ProviderRuntimeEventV2ThreadStateChangedPayloadState::Compacted
    )
}

/// `{...event, eventId, type: "thread.state.changed", payload}`: a new event that keeps the
/// envelope of `event` (thread, turn, item, request, raw, …) under another type.
pub fn retyped_as_thread_state_changed(event: &ProviderRuntimeEvent, event_id: EventId, payload: serde_json::Value) -> Option<ProviderRuntimeEvent> {
    let mut value = serde_json::to_value(event).ok()?;
    let object = value.as_object_mut()?;
    object.insert("eventId".into(), serde_json::Value::String(event_id.to_string()));
    object.insert("type".into(), serde_json::Value::String("thread.state.changed".into()));
    object.insert("payload".into(), payload);
    serde_json::from_value(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_and_writes_the_shared_envelope() {
        let mut event: ProviderRuntimeEvent = serde_json::from_value(json!({
            "type": "turn.completed",
            "eventId": "evt-1",
            "provider": "codex",
            "createdAt": "2026-01-01T00:00:00.000Z",
            "threadId": "thread-1",
            "turnId": "turn-1",
            "payload": {"state": "completed"}
        }))
        .unwrap();
        assert_eq!(event_type(&event), "turn.completed");
        assert_eq!(thread_id(&event).as_str(), "thread-1");
        assert_eq!(turn_id(&event).map(|t| t.as_str()), Some("turn-1"));
        assert_eq!(provider_instance_id(&event), None);
        set_provider_instance_id(&mut event, ProviderInstanceId::from("codex_work"));
        set_request_id(&mut event, RuntimeRequestId::from("req-1"));
        let compacted = retyped_as_thread_state_changed(&event, EventId::from("evt-1:context-compaction"), json!({"state": "compacted"})).unwrap();
        assert!(is_compacted(&compacted));
        assert_eq!(
            serde_json::to_value(&compacted).unwrap(),
            json!({
                "eventId": "evt-1:context-compaction",
                "provider": "codex",
                "providerInstanceId": "codex_work",
                "threadId": "thread-1",
                "createdAt": "2026-01-01T00:00:00.000Z",
                "turnId": "turn-1",
                "requestId": "req-1",
                "type": "thread.state.changed",
                "payload": {"state": "compacted"}
            })
        );
    }
}
