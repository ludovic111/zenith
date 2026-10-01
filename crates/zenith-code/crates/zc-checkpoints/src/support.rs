//! Small helpers shared by the modules: clock and ids, command decoding, the wire-shaped
//! provider placeholders of `zc_ports`, and the `packages/shared` bits this crate needs.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use zc_contracts::OrchestrationCommand;
use zc_ports::contracts::{ProviderRuntimeEvent, ProviderSession};

/// `DateTime.formatIso(DateTime.now)`.
pub fn now_iso() -> String {
    zc_core::time::now_iso()
}

/// `server:<tag>:<uuid>`.
pub fn server_command_id(tag: &str) -> String {
    zc_core::ids::server_command_id(tag)
}

/// `crypto.randomUUID()`.
pub fn uuid() -> String {
    zc_core::ids::uuid_v4()
}

/// A command built as wire JSON, decoded into the contract type. The builders in this crate
/// only produce valid commands, so a failure is a bug; it surfaces as an error message rather
/// than a panic.
pub fn decode_command(value: Value) -> Result<OrchestrationCommand, String> {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("?").to_owned();
    serde_json::from_value(value).map_err(|error| format!("invalid {kind} command: {error}"))
}

/// A string field of a wire object.
pub fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Accessors over the `ProviderSession` placeholder (`provider.ts`).
pub trait ProviderSessionExt {
    fn thread_id(&self) -> Option<&str>;
    fn cwd(&self) -> Option<&str>;
    fn status(&self) -> Option<&str>;
    fn active_turn_id(&self) -> Option<&str>;
}

impl ProviderSessionExt for ProviderSession {
    fn thread_id(&self) -> Option<&str> {
        str_field(&self.0, "threadId")
    }
    fn cwd(&self) -> Option<&str> {
        str_field(&self.0, "cwd")
    }
    fn status(&self) -> Option<&str> {
        str_field(&self.0, "status")
    }
    fn active_turn_id(&self) -> Option<&str> {
        str_field(&self.0, "activeTurnId")
    }
}

/// Accessors over the `ProviderRuntimeEvent` placeholder (`providerRuntime.ts`).
pub trait ProviderRuntimeEventExt {
    fn event_type(&self) -> &str;
    fn thread_id(&self) -> &str;
    /// `turnId` (`String(value)` of whatever is there).
    fn turn_id(&self) -> Option<String>;
    fn created_at(&self) -> &str;
    /// `payload.state`.
    fn payload_state(&self) -> Option<&str>;
}

impl ProviderRuntimeEventExt for ProviderRuntimeEvent {
    fn event_type(&self) -> &str {
        str_field(&self.0, "type").unwrap_or("")
    }
    fn thread_id(&self) -> &str {
        str_field(&self.0, "threadId").unwrap_or("")
    }
    fn turn_id(&self) -> Option<String> {
        match self.0.get("turnId") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(value.clone()),
            Some(other) => Some(other.to_string()),
        }
    }
    fn created_at(&self) -> &str {
        str_field(&self.0, "createdAt").unwrap_or("")
    }
    fn payload_state(&self) -> Option<&str> {
        self.0.get("payload").and_then(|payload| str_field(payload, "state"))
    }
}

/// `isTemporaryWorktreeBranch` (`packages/shared/src/git.ts`): `t3code/<8 hex>` or
/// `t3code/<uuid v4>`, the placeholder a first turn renames.
pub fn is_temporary_worktree_branch(ref_name: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| Regex::new(r"^t3code/(?:[0-9a-f]{8}|[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})$").expect("valid regex"))
        .is_match(&ref_name.trim().to_lowercase())
}

/// `sameId(left, right)`: both present and equal.
pub fn same_id(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(l), Some(r)) if l == r)
}

/// `path.relative(parent, child)` is `""` or stays inside `parent` (both canonical).
pub fn is_within(parent: &std::path::Path, child: &std::path::Path) -> bool {
    child.starts_with(parent)
}

/// `Number.prototype.toLocaleString("en-US")` for counts: thousands separators.
pub fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_branches_are_t3code_tokens() {
        assert!(is_temporary_worktree_branch("t3code/fd9cbe0e"));
        assert!(is_temporary_worktree_branch(" T3CODE/FD9CBE0E "));
        assert!(is_temporary_worktree_branch("t3code/4a1b2c3d-1234-4abc-8def-0123456789ab"));
        assert!(!is_temporary_worktree_branch("t3code/original-branch"));
        assert!(!is_temporary_worktree_branch("feature/x"));
    }

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1234567), "1,234,567");
    }
}
