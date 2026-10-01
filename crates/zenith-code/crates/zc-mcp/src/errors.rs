//! The tagged errors of the MCP tools (`previewAutomation.ts`, `McpInvocationContext.ts`,
//! `device.ts` contracts), with the messages agents read. Each is a [`TaggedError`]: the
//! encoded `{_tag, …fields}` plus the message the TypeScript class computes.

use serde_json::{json, Map, Value};
use zc_ports::TaggedError;

use crate::brand;
use crate::scope::{McpCapability, McpInvocationScope};

fn tagged(tag: &str, fields: Map<String, Value>, message: String) -> TaggedError {
    let mut error = TaggedError::new(tag, message);
    error.fields = fields;
    error
}

fn scope_fields(scope: &McpInvocationScope) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("environmentId".into(), json!(scope.environment_id));
    fields.insert("threadId".into(), json!(scope.thread_id));
    fields.insert("providerSessionId".into(), json!(scope.provider_session_id));
    fields.insert("providerInstanceId".into(), json!(scope.provider_instance_id));
    fields
}

/// `PreviewAutomationUnavailableError` (preview) or `McpCapabilityUnavailableError`.
pub fn capability_unavailable(scope: &McpInvocationScope, capability: McpCapability) -> TaggedError {
    let mut fields = Map::new();
    fields.insert("capability".into(), json!(capability.as_str()));
    fields.extend(scope_fields(scope));
    if capability == McpCapability::Preview {
        tagged(
            "PreviewAutomationUnavailableError",
            fields,
            "MCP credential does not grant the preview capability: browser preview tools are off for this thread. Do not retry them. To check a page, use a headless browser from the shell, such as Playwright, or curl. The user can turn on \"Agent browser access\" in Settings; it applies when the agent session next starts.".into(),
        )
    } else {
        tagged(
            "McpCapabilityUnavailableError",
            fields,
            format!("MCP credential does not grant the {} capability.", capability.as_str()),
        )
    }
}

/// `PreviewAutomationNoAvailableHostError` for a request no host could take.
pub fn no_available_host(operation: &str, scope: &McpInvocationScope) -> TaggedError {
    let mut fields = Map::new();
    fields.insert("operation".into(), json!(operation));
    fields.extend(scope_fields(scope));
    no_available_host_with(fields, operation, &scope.environment_id)
}

fn no_available_host_with(fields: Map<String, Value>, operation: &str, environment_id: &str) -> TaggedError {
    tagged(
        "PreviewAutomationNoAvailableHostError",
        fields,
        brand(&format!(
            "No preview automation host is available for {operation} in environment {environment_id}. Preview tools run in a T3 Code desktop app that is open and connected to this environment; a headless server has no browser of its own. Do not retry. To check a page, use a headless browser from the shell, such as Playwright, or curl, or ask the user to open this thread in the T3 Code desktop app."
        )),
    )
}

/// `PreviewAutomationRequestErrorContext`: the routed request an error is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestErrorContext {
    pub operation: String,
    pub environment_id: String,
    pub thread_id: String,
    pub provider_session_id: String,
    pub provider_instance_id: String,
    pub client_id: String,
    pub connection_id: String,
    pub request_id: String,
    pub tab_id: Option<String>,
    pub timeout_ms: u64,
    /// `locator` or `selector`, from the request input.
    pub selector_kind: Option<String>,
    pub selector_length: Option<u64>,
}

impl RequestErrorContext {
    fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("operation".into(), json!(self.operation));
        fields.insert("environmentId".into(), json!(self.environment_id));
        fields.insert("threadId".into(), json!(self.thread_id));
        fields.insert("providerSessionId".into(), json!(self.provider_session_id));
        fields.insert("providerInstanceId".into(), json!(self.provider_instance_id));
        fields.insert("clientId".into(), json!(self.client_id));
        fields.insert("connectionId".into(), json!(self.connection_id));
        fields.insert("requestId".into(), json!(self.request_id));
        if let Some(tab_id) = &self.tab_id {
            fields.insert("tabId".into(), json!(tab_id));
        }
        fields.insert("timeoutMs".into(), json!(self.timeout_ms));
        if let Some(kind) = &self.selector_kind {
            fields.insert("selectorKind".into(), json!(kind));
        }
        if let Some(length) = self.selector_length {
            fields.insert("selectorLength".into(), json!(length));
        }
        fields
    }

    fn with_remote(&self, remote: Option<&Value>) -> Map<String, Value> {
        let mut fields = self.fields();
        if let Some(remote) = remote {
            fields.extend(remote_diagnostics(remote));
        }
        fields
    }
}

/// `remoteDetailKind`.
fn remote_detail_kind(detail: &Value) -> &'static str {
    match detail {
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        Value::Object(_) => "object",
    }
}

/// The remote diagnostics every classified error carries: the remote tag, the length of its
/// message, the kind of its detail, and the error itself as the cause.
fn remote_diagnostics(remote: &Value) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert("remoteTag".into(), remote.get("_tag").cloned().unwrap_or(Value::Null));
    let message_length = remote.get("message").and_then(Value::as_str).map(|m| m.encode_utf16().count()).unwrap_or(0);
    fields.insert("remoteMessageLength".into(), json!(message_length));
    if let Some(detail) = remote.get("detail") {
        fields.insert("remoteDetailKind".into(), json!(remote_detail_kind(detail)));
    }
    fields.insert("cause".into(), remote.clone());
    fields
}

pub fn tab_not_found(context: &RequestErrorContext, remote: Option<&Value>) -> TaggedError {
    let message = match &context.tab_id {
        Some(tab_id) => format!(
            "Preview tab {tab_id} was not found for {}. Omit tabId to use the current tab, or call preview_open.",
            context.operation
        ),
        None => format!("No active preview tab was found for {}. Call preview_open first.", context.operation),
    };
    tagged("PreviewAutomationTabNotFoundError", context.with_remote(remote), message)
}

pub fn timeout(context: &RequestErrorContext, remote: Option<&Value>) -> TaggedError {
    tagged(
        "PreviewAutomationTimeoutError",
        context.with_remote(remote),
        format!("Preview automation {} timed out after {}ms.", context.operation, context.timeout_ms),
    )
}

pub fn client_disconnected(context: &RequestErrorContext) -> TaggedError {
    tagged(
        "PreviewAutomationClientDisconnectedError",
        context.fields(),
        format!("Preview automation client {} disconnected during {}.", context.client_id, context.operation),
    )
}

pub fn request_queue_closed(context: &RequestErrorContext) -> TaggedError {
    tagged(
        "PreviewAutomationRequestQueueClosedError",
        context.fields(),
        format!(
            "Preview automation client {} stopped accepting {} requests.",
            context.client_id, context.operation
        ),
    )
}

pub fn malformed_response(context: &RequestErrorContext) -> TaggedError {
    tagged(
        "PreviewAutomationMalformedResponseError",
        context.fields(),
        format!(
            "Preview automation client {} returned a malformed response for {}.",
            context.client_id, context.operation
        ),
    )
}

fn recording(tag: &str, thread_id: &str, cause: Option<&Value>, message: &str) -> TaggedError {
    let mut fields = Map::new();
    fields.insert("threadId".into(), json!(thread_id));
    if let Some(cause) = cause {
        fields.insert("cause".into(), cause.clone());
    }
    tagged(tag, fields, message.into())
}

pub fn recording_transfer(thread_id: &str, cause: Option<&Value>) -> TaggedError {
    recording(
        "PreviewAutomationRecordingTransferError",
        thread_id,
        cause,
        "Preview recording could not be saved to the agent environment. The saved copy remains on the desktop.",
    )
}

pub fn recording_desktop_update_required(thread_id: &str, cause: Option<&Value>) -> TaggedError {
    recording(
        "PreviewAutomationRecordingDesktopUpdateRequiredError",
        thread_id,
        cause,
        "Update the desktop app to transfer recordings. The recording remains on the desktop.",
    )
}

/// `classifyResponseError`: the server-side error for a host's `{_tag, message, detail?}`.
/// The message is always built here, never taken from the page or the renderer.
pub fn classify_response_error(context: &RequestErrorContext, remote: &Value) -> TaggedError {
    let tag = remote.get("_tag").and_then(Value::as_str).unwrap_or("");
    let operation = &context.operation;
    let client_id = &context.client_id;
    let detail = remote.get("detail").filter(|detail| detail.is_object());
    match tag {
        "PreviewAutomationRecordingDesktopUpdateRequiredError" => recording_desktop_update_required(&context.thread_id, Some(remote)),
        "PreviewAutomationRecordingTooLargeError" => recording(
            "PreviewAutomationRecordingTooLargeError",
            &context.thread_id,
            Some(remote),
            "The recording exceeds 50 MiB. The saved copy remains on the desktop.",
        ),
        "PreviewAutomationRecordingDeadlineExpiredError" => recording(
            "PreviewAutomationRecordingDeadlineExpiredError",
            &context.thread_id,
            Some(remote),
            "The recording transfer deadline expired. The saved copy remains on the desktop.",
        ),
        "PreviewAutomationRecordingTransferError" => recording_transfer(&context.thread_id, Some(remote)),
        "PreviewAutomationNoAvailableHostError" => no_available_host_with(context.with_remote(Some(remote)), operation, &context.environment_id),
        "PreviewAutomationUnsupportedClientError" => tagged(
            "PreviewAutomationUnsupportedClientError",
            context.with_remote(Some(remote)),
            format!("Preview automation client {client_id} does not support {operation}."),
        ),
        "PreviewAutomationTabNotFoundError" => tab_not_found(context, Some(remote)),
        "PreviewAutomationTimeoutError" => timeout(context, Some(remote)),
        "PreviewAutomationControlInterruptedError" => tagged(
            "PreviewAutomationControlInterruptedError",
            context.with_remote(Some(remote)),
            format!("Preview automation {operation} was interrupted on client {client_id}."),
        ),
        "PreviewAutomationInvalidSelectorError" => {
            let message = match (&context.selector_kind, context.selector_length) {
                (Some(kind), Some(length)) => {
                    format!("Preview automation {operation} received an invalid {kind} ({length} characters).")
                }
                _ => format!("Preview automation {operation} received an invalid selector."),
            };
            tagged("PreviewAutomationInvalidSelectorError", context.with_remote(Some(remote)), message)
        }
        "PreviewAutomationTargetNotEditableError" => {
            let remote_kind = detail
                .and_then(|detail| detail.get("selectorKind"))
                .and_then(Value::as_str)
                .filter(|kind| matches!(*kind, "focused-element" | "locator" | "selector"));
            let remote_length = detail
                .and_then(|detail| detail.get("selectorLength"))
                .and_then(Value::as_f64)
                .filter(|length| length.fract() == 0.0 && *length >= 0.0)
                .map(|length| length as u64);
            let kind = remote_kind.map(str::to_owned).or_else(|| context.selector_kind.clone());
            let length = remote_length.or(context.selector_length);
            let mut fields = context.with_remote(Some(remote));
            fields.remove("selectorKind");
            fields.remove("selectorLength");
            if let Some(kind) = &kind {
                fields.insert("selectorKind".into(), json!(kind));
            }
            if let Some(length) = length {
                fields.insert("selectorLength".into(), json!(length));
            }
            let message = match (kind.as_deref(), length) {
                (Some("focused-element"), _) => format!("Preview automation {operation} requires an editable focused element."),
                (Some(kind), Some(length)) => {
                    format!("Preview automation {operation} requires an editable {kind} ({length} characters).")
                }
                _ => format!("Preview automation {operation} requires an editable target."),
            };
            tagged("PreviewAutomationTargetNotEditableError", fields, message)
        }
        "PreviewAutomationResultTooLargeError" => {
            let maximum = detail
                .and_then(|detail| detail.get("maximumBytes"))
                .and_then(Value::as_f64)
                .filter(|bytes| bytes.fract() == 0.0 && *bytes > 0.0)
                .map(|bytes| bytes as u64);
            let mut fields = context.with_remote(Some(remote));
            if let Some(maximum) = maximum {
                fields.insert("maximumBytes".into(), json!(maximum));
            }
            let message = match maximum {
                Some(maximum) => format!("Preview automation {operation} produced a result larger than {maximum} bytes."),
                None => format!("Preview automation {operation} produced a result that is too large."),
            };
            tagged("PreviewAutomationResultTooLargeError", fields, message)
        }
        "PreviewAutomationUnavailableError" => tagged(
            "PreviewAutomationRemoteUnavailableError",
            context.with_remote(Some(remote)),
            format!("Preview automation {operation} is unavailable on client {client_id}."),
        ),
        _ => tagged(
            "PreviewAutomationExecutionError",
            context.with_remote(Some(remote)),
            format!("Preview automation {operation} failed on client {client_id}."),
        ),
    }
}

/// `DeviceToolUnavailableError`.
pub fn device_tool_unavailable(reason: &str) -> TaggedError {
    let mut fields = Map::new();
    fields.insert("reason".into(), json!(reason));
    tagged("DeviceToolUnavailableError", fields, reason.into())
}
