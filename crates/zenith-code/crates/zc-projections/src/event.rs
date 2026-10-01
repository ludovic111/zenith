//! The event as the projectors read it: the envelope fields they use and the **encoded**
//! payload (`serde_json::Value`).
//!
//! The TS projectors read decoded events (`Schema.decode(OrchestrationEvent)`) and their
//! repositories re-encode what they store. A decoded-then-encoded payload is exactly the
//! canonical wire value, so working on the encoded value of a typed
//! [`zc_contracts::OrchestrationEvent`] is equivalent, and it keeps absent (`undefined`) and
//! `null` apart the way the TS spreads (`…(x !== undefined ? {x} : {})`) need. Persisted rows
//! go through the typed contract first ([`ProjectionEvent::from_persisted`]), so decoding
//! defaults and validation apply as they do in TS.

use serde_json::Value;
use zc_contracts::OrchestrationEvent;
use zc_db::repos::event_store::PersistedEvent;
use zc_db::DbError;

/// One event, as the projection pipeline and the subscriptions read it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionEvent {
    pub sequence: i64,
    pub event_id: String,
    pub event_type: String,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub occurred_at: String,
    pub command_id: Option<String>,
    pub metadata: Value,
    pub payload: Value,
}

impl ProjectionEvent {
    /// From a typed event (what the engine holds after appending).
    pub fn from_contract(event: &OrchestrationEvent) -> Result<Self, DbError> {
        let value = serde_json::to_value(event).map_err(|error| DbError::decode("ProjectionPipeline.projectEvent:encodeEvent", error.to_string()))?;
        Self::from_encoded(value, "ProjectionPipeline.projectEvent:encodeEvent")
    }

    /// From a stored row: decoded with the contract (as `OrchestrationEventStore` does in TS),
    /// then re-encoded.
    pub fn from_persisted(event: &PersistedEvent) -> Result<Self, DbError> {
        let typed = decode_persisted(event)?;
        Self::from_contract(&typed)
    }

    /// From an already canonical encoded event (the wire form of `OrchestrationEvent`).
    pub fn from_encoded(value: Value, operation: &str) -> Result<Self, DbError> {
        let field = |key: &str| -> Result<String, DbError> {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| DbError::decode(operation, format!("{key}: MissingKey")))
        };
        Ok(Self {
            sequence: value
                .get("sequence")
                .and_then(Value::as_i64)
                .ok_or_else(|| DbError::decode(operation, "sequence: MissingKey"))?,
            event_id: field("eventId")?,
            event_type: field("type")?,
            aggregate_kind: field("aggregateKind")?,
            aggregate_id: field("aggregateId")?,
            occurred_at: field("occurredAt")?,
            command_id: value.get("commandId").and_then(Value::as_str).map(str::to_owned),
            metadata: value.get("metadata").cloned().unwrap_or(Value::Null),
            payload: value.get("payload").cloned().unwrap_or(Value::Null),
        })
    }

    /// `event.payload.threadId`.
    pub fn thread_id(&self) -> Option<&str> {
        str_field(&self.payload, "threadId")
    }
}

/// A stored row decoded into the typed event (`decodeUnknown(OrchestrationEvent)`).
pub fn decode_persisted(event: &PersistedEvent) -> Result<OrchestrationEvent, DbError> {
    let value = serde_json::to_value(event).map_err(|error| DbError::decode("OrchestrationEventStore.readFromSequence:decodeRows", error.to_string()))?;
    serde_json::from_value(value).map_err(|error| {
        DbError::decode(
            "OrchestrationEventStore.readFromSequence:decodeRows",
            format!("event {}: {error}", event.sequence),
        )
    })
}

/// `value[key]` when it is a string.
pub fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `value[key] !== undefined` on an encoded value: the key is present (a `null` counts).
pub fn has_key(value: &Value, key: &str) -> bool {
    value.as_object().is_some_and(|object| object.contains_key(key))
}

/// `value[key]` as `string | null` (absent and `null` both give `None`).
pub fn opt_string(value: &Value, key: &str) -> Option<String> {
    str_field(value, key).map(str::to_owned)
}

/// `value[key]` as an encoded `X | null` (absent and `null` both give `None`).
pub fn opt_value(value: &Value, key: &str) -> Option<Value> {
    value.get(key).filter(|value| !value.is_null()).cloned()
}

/// `typeof value === "object" && value !== null` (arrays included, like JS) gives the record.
pub fn as_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}
