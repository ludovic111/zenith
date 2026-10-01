//! The repositories of `apps/server/src/persistence/**` (plan §3.1). Each module holds the
//! row types and functions taking `&Conn`, so a call joins whatever transaction the caller has
//! open (as an Effect `SqlClient` call does). From async code, run them through the actor:
//!
//! ```ignore
//! let thread = db.call(move |c| projection_threads::get_by_id(c, &thread_id)).await?;
//! db.transaction(move |c| {
//!     let event = event_store::append(c, &new_event)?;
//!     projection_state::upsert_many(c, &cursors)?;
//!     Ok(event)
//! }).await?;
//! ```
//!
//! **Contract types.** JSON columns (`payload_json`, `metadata_json`, `*_json`) are carried as
//! `serde_json::Value` and literal unions (`status`, `role`, `tone`, …) as `String`, so this
//! crate does not depend on `zc-contracts` yet. They hold the *encoded* (wire) form that the TS
//! server writes. When `zc-contracts` lands, swap each `Value`/`String` field for the generated
//! type (the field docs name it) and decode in the row mappers; the SQL does not change.
//! Callers must already pass wire-encoded values (e.g. an encoded `ProjectIconOverride`).

pub mod auth_pairing_links;
pub mod auth_sessions;
pub mod command_receipts;
pub mod event_store;
pub mod pending_approvals;
pub mod projection_state;
pub mod projects;
pub mod proposed_plans;
pub mod provider_session_runtime;
pub mod pull_request_files_viewed;
pub mod thread_activities;
pub mod thread_messages;
pub mod thread_pull_requests;
pub mod thread_sessions;
pub mod threads;
pub mod turns;

use serde_json::Value;

use crate::error::{Raw, RawResult};

// Literal unions a row is checked against on read, as the TS row schemas do (a row outside
// them is a decode error). zc-contracts will replace these with enums.

/// `RuntimeMode`.
pub const RUNTIME_MODES: &[&str] = &["approval-required", "auto-accept-edits", "auto", "full-access"];
/// `ProviderInteractionMode`.
pub const INTERACTION_MODES: &[&str] = &["default", "plan"];
/// `OrchestrationMessageRole`.
pub const MESSAGE_ROLES: &[&str] = &["user", "assistant", "system", "reasoning"];
/// `OrchestrationSessionStatus`.
pub const SESSION_STATUSES: &[&str] = &["idle", "starting", "running", "ready", "interrupted", "stopped", "error"];
/// `OrchestrationCheckpointStatus`.
pub const CHECKPOINT_STATUSES: &[&str] = &["ready", "missing", "error"];

/// `JSON.stringify` of an encoded value. serde_json writes the same compact text for the
/// values the server stores (no `undefined`, no non-finite numbers).
pub(crate) fn to_json(value: &Value) -> String {
    serde_json::to_string(value).expect("serializing a serde_json::Value cannot fail")
}

/// `Schema.fromJsonString(Schema.Unknown)` on a stored column.
pub(crate) fn parse_json(field: &str, text: &str) -> RawResult<Value> {
    serde_json::from_str(text).map_err(|_| Raw::Decode(format!("{field}: Encoding(InvalidValue)")))
}

/// `Schema.NullOr(Schema.fromJsonString(…))` on a stored column.
pub(crate) fn parse_json_opt(field: &str, text: Option<String>) -> RawResult<Option<Value>> {
    text.map(|text| parse_json(field, &text)).transpose()
}

/// A literal union column (`Schema.Literals([...])`): the stored text must be one of them.
pub(crate) fn literal(field: &str, value: String, allowed: &[&str]) -> RawResult<String> {
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(Raw::Decode(format!("{field}: InvalidValue")))
    }
}

/// A non-negative integer column (`NonNegativeInt`).
pub(crate) fn non_negative(field: &str, value: i64) -> RawResult<i64> {
    if value >= 0 {
        Ok(value)
    } else {
        Err(Raw::Decode(format!("{field}: Filter(InvalidValue)")))
    }
}
