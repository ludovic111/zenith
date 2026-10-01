//! The errors of the app-server client (`packages/effect-codex-app-server/src/errors.ts`) and of
//! the session runtime (`CodexSessionRuntime.ts`). The messages are the TS ones: they reach the
//! orchestration as `ProviderAdapterRequestError.detail` and the provider status as probe text.

use serde_json::Value;

/// `CodexAppServerRequestOperation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestOperation {
    DecodePayload,
    EncodePayload,
    HandleRequest,
    ReceiveResponse,
}

impl RequestOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestOperation::DecodePayload => "decode-payload",
            RequestOperation::EncodePayload => "encode-payload",
            RequestOperation::HandleRequest => "handle-request",
            RequestOperation::ReceiveResponse => "receive-response",
        }
    }
}

/// `CodexAppServerRequestError`: a JSON-RPC error, ours or the peer's.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
    pub method: Option<String>,
    pub request_id: Option<String>,
    pub operation: Option<RequestOperation>,
}

impl RequestError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
            method: None,
            request_id: None,
            operation: None,
        }
    }

    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(-32700, message)
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("Method not found: {method}"))
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    pub fn internal_error(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    pub fn overloaded(message: impl Into<String>) -> Self {
        Self::new(-32001, message)
    }

    /// `invalidPayload`: a payload that does not match its schema.
    pub fn invalid_payload(method: &str, operation: RequestOperation, detail: &str) -> Self {
        Self {
            code: -32602,
            message: format!("Invalid payload for method '{method}' during '{}'", operation.as_str()),
            data: Some(serde_json::json!({ "issue": detail })),
            method: Some(method.to_owned()),
            request_id: None,
            operation: Some(operation),
        }
    }

    /// `fromProtocolError`: the peer answered a request of ours with an error.
    pub fn from_protocol_error(code: i64, message: String, data: Option<Value>, method: &str, request_id: &str) -> Self {
        Self {
            code,
            message,
            data,
            method: Some(method.to_owned()),
            request_id: Some(request_id.to_owned()),
            operation: Some(RequestOperation::ReceiveResponse),
        }
    }

    /// The JSON-RPC `error` object written back to the peer.
    pub fn to_protocol_error(&self) -> Value {
        let mut error = serde_json::Map::new();
        error.insert("code".into(), Value::from(self.code));
        error.insert("message".into(), Value::from(self.message.clone()));
        if let Some(data) = &self.data {
            error.insert("data".into(), data.clone());
        }
        Value::Object(error)
    }
}

/// `CodexAppServerProtocolParseOperation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolParseOperation {
    EncodeWireMessage,
    DecodeWireMessage,
    RouteWireMessage,
    DecodeNotificationPayload,
    DecodeRequestPayload,
    DecodeResponsePayload,
}

impl ProtocolParseOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            ProtocolParseOperation::EncodeWireMessage => "encode-wire-message",
            ProtocolParseOperation::DecodeWireMessage => "decode-wire-message",
            ProtocolParseOperation::RouteWireMessage => "route-wire-message",
            ProtocolParseOperation::DecodeNotificationPayload => "decode-notification-payload",
            ProtocolParseOperation::DecodeRequestPayload => "decode-request-payload",
            ProtocolParseOperation::DecodeResponsePayload => "decode-response-payload",
        }
    }
}

/// `CodexAppServerTransportOperation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportOperation {
    ReadInputStream,
    ReadProcessExitStatus,
}

impl TransportOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            TransportOperation::ReadInputStream => "read-input-stream",
            TransportOperation::ReadProcessExitStatus => "read-process-exit-status",
        }
    }
}

/// `CodexAppServerError`, the union of the client's tagged errors.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CodexAppServerError {
    #[error("{}", .0.message)]
    Request(RequestError),
    #[error("{}", match command { Some(command) => format!("Failed to spawn Codex App Server process for command: {command}"), None => "Failed to spawn Codex App Server process".to_owned() })]
    Spawn { command: Option<String>, cause: String },
    #[error("{}", match code { Some(code) => format!("Codex App Server process exited with code {code}"), None => "Codex App Server process exited".to_owned() })]
    ProcessExited { code: Option<i32>, pid: Option<u32> },
    #[error("Codex App Server protocol operation '{}' failed{}.", operation.as_str(), method.as_ref().map(|method| format!(" for method '{method}'")).unwrap_or_default())]
    ProtocolParse {
        operation: ProtocolParseOperation,
        method: Option<String>,
        request_id: Option<String>,
        /// Structural detail only (no payload values), like the TS diagnostics.
        detail: Option<String>,
    },
    #[error("Codex App Server transport operation '{}' failed.", operation.as_str())]
    Transport {
        operation: TransportOperation,
        pid: Option<u32>,
        cause: String,
    },
    #[error("Codex App Server input stream ended.")]
    InputStreamEnded,
}

impl CodexAppServerError {
    /// The TS `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            CodexAppServerError::Request(_) => "CodexAppServerRequestError",
            CodexAppServerError::Spawn { .. } => "CodexAppServerSpawnError",
            CodexAppServerError::ProcessExited { .. } => "CodexAppServerProcessExitedError",
            CodexAppServerError::ProtocolParse { .. } => "CodexAppServerProtocolParseError",
            CodexAppServerError::Transport { .. } => "CodexAppServerTransportError",
            CodexAppServerError::InputStreamEnded => "CodexAppServerInputStreamEndedError",
        }
    }

    pub fn as_request(&self) -> Option<&RequestError> {
        match self {
            CodexAppServerError::Request(error) => Some(error),
            _ => None,
        }
    }

    /// `fromAppServerError`: the error a failed incoming-request handler answers with.
    pub fn to_request_error(&self, method: &str) -> RequestError {
        match self {
            CodexAppServerError::Request(error) => error.clone(),
            other => {
                let mut error = RequestError::internal_error(format!("Codex App Server request handler failed for method '{method}'"));
                error.method = Some(method.to_owned());
                error.operation = Some(RequestOperation::HandleRequest);
                error.data = Some(serde_json::json!({ "causeTag": other.tag() }));
                error
            }
        }
    }
}

impl From<RequestError> for CodexAppServerError {
    fn from(error: RequestError) -> Self {
        CodexAppServerError::Request(error)
    }
}

/// `CodexSessionRuntimeError`: the app-server errors plus the runtime's own.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CodexSessionRuntimeError {
    #[error(transparent)]
    AppServer(#[from] CodexAppServerError),
    #[error("Unknown pending Codex approval request: {request_id}")]
    PendingApprovalNotFound { request_id: String },
    #[error("Unknown pending Codex user input request: {request_id}")]
    PendingUserInputNotFound { request_id: String },
    #[error("Invalid Codex user input answers for question '{question_id}'")]
    InvalidUserInputAnswers { question_id: String },
    #[error("Codex session is missing a provider thread id for {thread_id}")]
    ThreadIdMissing { thread_id: String },
}

impl From<RequestError> for CodexSessionRuntimeError {
    fn from(error: RequestError) -> Self {
        CodexSessionRuntimeError::AppServer(CodexAppServerError::Request(error))
    }
}

impl CodexSessionRuntimeError {
    pub fn tag(&self) -> &'static str {
        match self {
            CodexSessionRuntimeError::AppServer(error) => error.tag(),
            CodexSessionRuntimeError::PendingApprovalNotFound { .. } => "CodexSessionRuntimePendingApprovalNotFoundError",
            CodexSessionRuntimeError::PendingUserInputNotFound { .. } => "CodexSessionRuntimePendingUserInputNotFoundError",
            CodexSessionRuntimeError::InvalidUserInputAnswers { .. } => "CodexSessionRuntimeInvalidUserInputAnswersError",
            CodexSessionRuntimeError::ThreadIdMissing { .. } => "CodexSessionRuntimeThreadIdMissingError",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_the_typescript_errors() {
        assert_eq!(
            CodexAppServerError::Spawn {
                command: Some("codex app-server".into()),
                cause: String::new()
            }
            .to_string(),
            "Failed to spawn Codex App Server process for command: codex app-server"
        );
        assert_eq!(
            CodexAppServerError::ProcessExited { code: Some(2), pid: Some(7) }.to_string(),
            "Codex App Server process exited with code 2"
        );
        assert_eq!(
            CodexAppServerError::ProcessExited { code: None, pid: None }.to_string(),
            "Codex App Server process exited"
        );
        assert_eq!(
            CodexAppServerError::ProtocolParse {
                operation: ProtocolParseOperation::DecodeResponsePayload,
                method: Some("turn/start".into()),
                request_id: None,
                detail: None
            }
            .to_string(),
            "Codex App Server protocol operation 'decode-response-payload' failed for method 'turn/start'."
        );
        assert_eq!(CodexAppServerError::InputStreamEnded.to_string(), "Codex App Server input stream ended.");
        assert_eq!(
            RequestError::method_not_found("x/test").to_protocol_error(),
            serde_json::json!({"code": -32601, "message": "Method not found: x/test"})
        );
        assert_eq!(
            CodexSessionRuntimeError::ThreadIdMissing { thread_id: "t".into() }.to_string(),
            "Codex session is missing a provider thread id for t"
        );
    }
}
