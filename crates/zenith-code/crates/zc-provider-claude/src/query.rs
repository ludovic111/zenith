//! The seam between the adapter and the CLI: what the SDK's `query()` returned in TS
//! (`ClaudeQueryRuntime`), with the `canUseTool` / `onUserDialog` callbacks as a trait. The
//! real implementation is [`crate::protocol::ProcessQueryFactory`]; tests plug in fakes.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::options::ClaudeQueryOptions;

/// A query failure (spawn, stream, control request). The message is what the SDK's error
/// message would be; the adapter inspects it to tell interrupts from failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct QueryError {
    pub message: String,
}

impl QueryError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

/// The user messages the adapter queues (`SDKUserMessage` JSON). Dropping the sender ends the
/// prompt stream.
pub type PromptSender = mpsc::UnboundedSender<Value>;
pub type PromptReceiver = mpsc::UnboundedReceiver<Value>;

/// The SDK messages a query yields, then `Err` on a failed stream; the channel closing is the
/// clean end.
pub type MessageReceiver = mpsc::UnboundedReceiver<Result<Value, QueryError>>;

/// One `can_use_tool` control request.
#[derive(Debug, Clone, PartialEq)]
pub struct CanUseToolRequest {
    pub tool_name: String,
    pub input: Value,
    /// `permission_suggestions`.
    pub suggestions: Option<Vec<Value>>,
    pub tool_use_id: Option<String>,
    pub agent_id: Option<String>,
    pub request_id: String,
}

/// A `PermissionResult`.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionResult {
    Allow {
        updated_input: Value,
        updated_permissions: Option<Vec<Value>>,
    },
    Deny {
        message: String,
    },
}

impl PermissionResult {
    /// The control-response body: the result plus the request's `toolUseID`.
    pub fn to_response(&self, tool_use_id: Option<&str>) -> Value {
        let mut body = Map::new();
        match self {
            Self::Allow {
                updated_input,
                updated_permissions,
            } => {
                body.insert("behavior".into(), Value::String("allow".into()));
                body.insert("updatedInput".into(), updated_input.clone());
                if let Some(permissions) = updated_permissions {
                    body.insert("updatedPermissions".into(), Value::Array(permissions.clone()));
                }
            }
            Self::Deny { message } => {
                body.insert("behavior".into(), Value::String("deny".into()));
                body.insert("message".into(), Value::String(message.clone()));
            }
        }
        if let Some(id) = tool_use_id {
            body.insert("toolUseID".into(), Value::String(id.to_string()));
        }
        Value::Object(body)
    }
}

/// One `request_user_dialog` control request.
#[derive(Debug, Clone, PartialEq)]
pub struct UserDialogRequest {
    pub dialog_kind: String,
    pub payload: Value,
    pub tool_use_id: Option<String>,
    pub request_id: String,
}

/// The adapter's side of the control protocol. `cancel` fires when the CLI sends
/// `control_cancel_request` for this request (the SDK's abort signal).
#[async_trait]
pub trait QueryCallbacks: Send + Sync {
    async fn can_use_tool(&self, request: CanUseToolRequest, cancel: CancellationToken) -> PermissionResult;
    /// `{behavior: "cancelled"}` / `{behavior: "completed", result}`; `None` stays silent.
    async fn on_user_dialog(&self, request: UserDialogRequest, cancel: CancellationToken) -> Option<Value>;
}

/// `ClaudeQueryRuntime`: the live query's controls.
#[async_trait]
pub trait ClaudeQueryRuntime: Send + Sync {
    /// Whether `interrupt` is available (the TS test doubles have none).
    fn supports_interrupt(&self) -> bool {
        true
    }
    /// `Query.interrupt`: ask Claude to abort the running turn.
    async fn interrupt(&self) -> Result<(), QueryError>;
    async fn set_model(&self, model: Option<&str>) -> Result<(), QueryError>;
    async fn set_permission_mode(&self, mode: &str) -> Result<(), QueryError>;
    /// Close stdin and terminate the process (SIGTERM after 2 s, SIGKILL 5 s later).
    fn close(&self) -> Result<(), QueryError>;
}

/// A started query.
pub struct ClaudeQueryHandle {
    pub runtime: Arc<dyn ClaudeQueryRuntime>,
    pub messages: MessageReceiver,
}

/// Creates queries (`createQuery` in TS).
pub trait ClaudeQueryFactory: Send + Sync {
    fn create(&self, options: ClaudeQueryOptions, prompt: PromptReceiver, callbacks: Arc<dyn QueryCallbacks>) -> Result<ClaudeQueryHandle, QueryError>;
}
