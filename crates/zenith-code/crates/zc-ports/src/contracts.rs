//! Placeholders for the generated contract types (WP-01, crate `zc-contracts`).
//!
//! Every type here stands for exactly one type of `code/packages/contracts/src` (named in its
//! doc) and serializes to the same JSON:
//!
//! - **Ids** are `#[serde(transparent)]` string newtypes, like the generator emits for branded
//!   strings (plan §1.5).
//! - **Payloads** are `#[serde(transparent)]` newtypes over `serde_json::Value`: distinct types
//!   (so a `ProviderSession` cannot be passed where a `ProviderSendTurnInput` is expected) that
//!   carry the wire-encoded value unchanged.
//! - **Errors** are [`TaggedError`] aliases: the encoded `{"_tag": …, …fields}` object plus the
//!   human message.
//!
//! **Swapping in the real types:** replace each `placeholder!`/`id!` line (and each error alias)
//! with `pub use zc_contracts::<Name>;` (or the owning crate's error enum). Ports and callers
//! keep compiling wherever they only pass values through; code that builds or inspects a
//! placeholder's `.0` JSON becomes a compile error that points at what to rewrite.
//!
//! **Swapped so far** (WP-08): the branded ids (except `RpcClientId`, a string here but a
//! number in the contracts) and every orchestration type are the real `zc_contracts` types.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

macro_rules! id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

macro_rules! placeholder {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Value);

        impl From<Value> for $name {
            fn from(value: Value) -> Self {
                Self(value)
            }
        }
    };
}

/// Generated contract types (zc-contracts) that replaced their placeholders: the branded ids and
/// the orchestration read side (swapped by WP-09).
pub use zc_contracts::{
    AgentSessionImportSource, ApprovalRequestId, AuthSessionId, CheckpointRef, CommandId, EnvironmentId, EventId, MessageId, OrchestrationCheckpointSummary,
    OrchestrationClientOrigin, OrchestrationCommand, OrchestrationEvent, OrchestrationMessage, OrchestrationProject, OrchestrationProjectShell,
    OrchestrationReadModel, OrchestrationSearchThreadsInput, OrchestrationSearchThreadsResult, OrchestrationShellSnapshot, OrchestrationThread,
    OrchestrationThreadActivity, OrchestrationThreadDetailSnapshot, OrchestrationThreadDetailWindow, OrchestrationThreadShell, ProjectId, ProviderDriverKind,
    ProviderInstanceId, ThreadId, TurnId,
};

// ---------------------------------------------------------------------------------------------
// Branded ids (`baseSchemas.ts`, `providerInstance.ts`, `auth.ts`, `environment.ts`)
// ---------------------------------------------------------------------------------------------

id!(
    /// `RpcClientId` (`background.ts`).
    RpcClientId
);

// ---------------------------------------------------------------------------------------------
// Providers (`provider.ts`, `providerRuntime.ts`, `model.ts`, `server.ts`, `orchestration.ts`)
// ---------------------------------------------------------------------------------------------

placeholder!(
    /// `ProviderSessionStartInput` (`provider.ts`).
    ProviderSessionStartInput
);
placeholder!(
    /// `ProviderSession` (`provider.ts`).
    ProviderSession
);
placeholder!(
    /// `ProviderSendTurnInput` (`provider.ts`).
    ProviderSendTurnInput
);
placeholder!(
    /// `ProviderTurnStartResult` (`provider.ts`).
    ProviderTurnStartResult
);
placeholder!(
    /// `ProviderInterruptTurnInput` (`provider.ts`).
    ProviderInterruptTurnInput
);
placeholder!(
    /// `ProviderRespondToRequestInput` (`provider.ts`).
    ProviderRespondToRequestInput
);
placeholder!(
    /// `ProviderRespondToUserInputInput` (`provider.ts`).
    ProviderRespondToUserInputInput
);
placeholder!(
    /// `ProviderStopSessionInput` (`provider.ts`).
    ProviderStopSessionInput
);
placeholder!(
    /// `ProviderUploadFeedbackInput` (`provider.ts`).
    ProviderUploadFeedbackInput
);
placeholder!(
    /// `ProviderUploadFeedbackResult` (`provider.ts`).
    ProviderUploadFeedbackResult
);
placeholder!(
    /// `ProviderRuntimeEvent` (`providerRuntime.ts`, 48 `type`s). Internal to the server, so the
    /// Rust port may model it freely, but keeping the TS JSON keeps the `CANON:` log fixtures.
    ProviderRuntimeEvent
);
placeholder!(
    /// `ModelSelection` (`orchestration.ts`): `{instanceId, model, options?}`.
    ModelSelection
);
placeholder!(
    /// `ChatAttachment` (`orchestration.ts`).
    ChatAttachment
);
placeholder!(
    /// `ServerProvider` (`server.ts`): one provider instance's status snapshot.
    ServerProvider
);

// Orchestration (`orchestration.ts`, `agentSessions.ts`): generated, see the `pub use` above.

// ---------------------------------------------------------------------------------------------
// Terminal (`terminal.ts`)
// ---------------------------------------------------------------------------------------------

placeholder!(
    /// `TerminalOpenInput` (`terminal.ts`).
    TerminalOpenInput
);
placeholder!(
    /// `TerminalAttachInput` (`terminal.ts`).
    TerminalAttachInput
);
placeholder!(
    /// `TerminalWriteInput` (`terminal.ts`).
    TerminalWriteInput
);
placeholder!(
    /// `TerminalResizeInput` (`terminal.ts`).
    TerminalResizeInput
);
placeholder!(
    /// `TerminalClearInput` (`terminal.ts`).
    TerminalClearInput
);
placeholder!(
    /// `TerminalRestartInput` (`terminal.ts`).
    TerminalRestartInput
);
placeholder!(
    /// `TerminalCloseInput` (`terminal.ts`).
    TerminalCloseInput
);
placeholder!(
    /// `TerminalSessionSnapshot` (`terminal.ts`).
    TerminalSessionSnapshot
);
placeholder!(
    /// `TerminalAttachStreamEvent` (`terminal.ts`): the snapshot first, then output/exit events.
    TerminalAttachStreamEvent
);
placeholder!(
    /// `TerminalEvent` (`terminal.ts`).
    TerminalEvent
);
placeholder!(
    /// `TerminalMetadataStreamEvent` (`terminal.ts`).
    TerminalMetadataStreamEvent
);

// ---------------------------------------------------------------------------------------------
// Git and VCS (`git.ts`, `vcs.ts`)
// ---------------------------------------------------------------------------------------------

placeholder!(
    /// `VcsStatusInput` (`git.ts`): `{cwd}`.
    VcsStatusInput
);
placeholder!(
    /// `VcsStatusResult` (`git.ts`).
    VcsStatusResult
);
placeholder!(
    /// `VcsStatusLocalResult` (`git.ts`).
    VcsStatusLocalResult
);
placeholder!(
    /// `VcsStatusRemoteResult` (`git.ts`).
    VcsStatusRemoteResult
);
placeholder!(
    /// `NonNullable<VcsStatusResult["pr"]>` (`git.ts`): the branch's change request.
    VcsStatusPullRequest
);
placeholder!(
    /// `VcsPullResult` (`git.ts`).
    VcsPullResult
);
placeholder!(
    /// `GitRunStackedActionInput` (`git.ts`).
    GitRunStackedActionInput
);
placeholder!(
    /// `GitRunStackedActionResult` (`git.ts`).
    GitRunStackedActionResult
);
placeholder!(
    /// `GitActionProgressEvent` (`git.ts`).
    GitActionProgressEvent
);
placeholder!(
    /// `GitPullRequestRefInput` (`git.ts`).
    GitPullRequestRefInput
);
placeholder!(
    /// `GitResolvePullRequestResult` (`git.ts`).
    GitResolvePullRequestResult
);
placeholder!(
    /// `GitPreparePullRequestThreadInput` (`git.ts`).
    GitPreparePullRequestThreadInput
);
placeholder!(
    /// `GitPreparePullRequestThreadResult` (`git.ts`).
    GitPreparePullRequestThreadResult
);
placeholder!(
    /// `VcsListRefsInput` (`git.ts`).
    VcsListRefsInput
);
placeholder!(
    /// `VcsListRefsResult` (`git.ts`).
    VcsListRefsResult
);
placeholder!(
    /// `VcsCreateWorktreeInput` (`git.ts`).
    VcsCreateWorktreeInput
);
placeholder!(
    /// `VcsCreateWorktreeResult` (`git.ts`).
    VcsCreateWorktreeResult
);
placeholder!(
    /// `VcsRemoveWorktreeInput` (`git.ts`).
    VcsRemoveWorktreeInput
);
placeholder!(
    /// `VcsCreateRefInput` (`git.ts`).
    VcsCreateRefInput
);
placeholder!(
    /// `VcsCreateRefResult` (`git.ts`).
    VcsCreateRefResult
);
placeholder!(
    /// `VcsSwitchRefInput` (`git.ts`).
    VcsSwitchRefInput
);
placeholder!(
    /// `VcsSwitchRefResult` (`git.ts`).
    VcsSwitchRefResult
);

/// `WorktreeSubmodules` (`environment.ts`): `"recursive" | "top-level" | "none"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorktreeSubmodules {
    Recursive,
    TopLevel,
    None,
}

// ---------------------------------------------------------------------------------------------
// Pull requests (`pullRequest.ts`)
// ---------------------------------------------------------------------------------------------

placeholder!(
    /// `PullRequestRef` (`pullRequest.ts`): `{projectId, host?, expectedAccountId?, allowStale?,
    /// repository, number}`.
    PullRequestRef
);
placeholder!(
    /// `PullRequestSummary` (`pullRequest.ts`).
    PullRequestSummary
);
placeholder!(
    /// `PullRequestStack` (`pullRequest.ts`).
    PullRequestStack
);
placeholder!(
    /// `PullRequestInvalidateInput` (`pullRequest.ts`).
    PullRequestInvalidateInput
);
placeholder!(
    /// `PullRequestDiffInput` (`pullRequest.ts`).
    PullRequestDiffInput
);
placeholder!(
    /// `PullRequestDiffResult` (`pullRequest.ts`).
    PullRequestDiffResult
);

// ---------------------------------------------------------------------------------------------
// Settings and background (`settings.ts`, `background.ts`)
// ---------------------------------------------------------------------------------------------

/// `ServerSettings` and `ServerSettingsPatch` (`settings.ts`): the generated types.
pub use zc_contracts::{ServerSettings, ServerSettingsPatch};
placeholder!(
    /// `BackgroundPolicySnapshot` (`background.ts`).
    BackgroundPolicySnapshot
);
placeholder!(
    /// `ClientActivityReportInput` (`background.ts`).
    ClientActivityReportInput
);
placeholder!(
    /// `HostPowerSnapshot` (`background.ts`).
    HostPowerSnapshot
);

/// `BackgroundScope` (`background.ts`), concrete because pollers build it to ask the policy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum BackgroundScope {
    ServerConfig,
    ProviderStatus {
        #[serde(rename = "instanceId", default, skip_serializing_if = "Option::is_none")]
        instance_id: Option<ProviderInstanceId>,
    },
    VcsStatus {
        cwd: String,
    },
    GitRefs {
        cwd: String,
    },
    Diagnostics,
    Thread {
        #[serde(rename = "threadId")]
        thread_id: ThreadId,
    },
}

impl BackgroundScope {
    /// `scopeKey` of `BackgroundPolicy.ts`, used to group leases.
    pub fn key(&self) -> String {
        match self {
            Self::ServerConfig => "server-config".to_owned(),
            Self::Diagnostics => "diagnostics".to_owned(),
            Self::ProviderStatus { instance_id: None } => "provider-status".to_owned(),
            Self::ProviderStatus { instance_id: Some(id) } => format!("provider-status:{id}"),
            Self::VcsStatus { cwd } => format!("vcs-status:{cwd}"),
            Self::GitRefs { cwd } => format!("git-refs:{cwd}"),
            Self::Thread { thread_id } => format!("thread:{thread_id}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// A wire-encoded tagged error (`Schema.TaggedError`): `{"_tag": "<Name>", …declared fields}`.
/// The human `message` (a getter in TS, never serialized) travels alongside.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaggedError {
    #[serde(rename = "_tag")]
    pub tag: String,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
    #[serde(skip)]
    pub message: String,
}

impl TaggedError {
    pub fn new(tag: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            tag: tag.into(),
            fields: Map::new(),
            message: message.into(),
        }
    }

    /// Add a declared field.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    pub fn is(&self, tag: &str) -> bool {
        self.tag == tag
    }
}

impl std::fmt::Display for TaggedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            f.write_str(&self.tag)
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for TaggedError {}

/// `ProviderServiceError` (`apps/server/src/provider/Errors.ts`): validation, session-not-found,
/// session-closed, request, process, workspace-missing, unsupported, instance-not-found,
/// driver and session-directory errors. Becomes an enum owned by zc-providers.
pub type ProviderServiceError = TaggedError;
/// `ProviderSetupError` (`providerSetup.ts`).
pub type ProviderSetupError = TaggedError;
/// `OrchestrationDispatchError` (`apps/server/src/orchestration/Errors.ts`): command invariant,
/// settle blocked, previously rejected, command-id conflict, projector decode, plus persistence
/// errors. Becomes an enum owned by zc-orchestration.
pub type OrchestrationDispatchError = TaggedError;
/// `PersistenceSqlError | PersistenceDecodeError` (`apps/server/src/persistence/Errors.ts`),
/// a.k.a. `ProjectionRepositoryError` / `OrchestrationEventStoreError`. Owned by zc-db.
pub type PersistenceError = TaggedError;
/// `TerminalError` (`terminal.ts`).
pub type TerminalError = TaggedError;
/// `GitCommandError` (`git.ts`).
pub type GitCommandError = TaggedError;
/// `GitManagerServiceError` (`git.ts`): `GitManagerError | GitPullRequestMaterializationError |
/// GitCommandError | SourceControlProviderError | TextGenerationError`.
pub type GitManagerServiceError = TaggedError;
/// `TextGenerationError` (`git.ts`): `{operation, detail, cause?}`.
pub type TextGenerationError = TaggedError;
/// `PullRequestUnavailableError | PullRequestOperationError` (`pullRequest.ts`).
pub type PullRequestError = TaggedError;
/// `ServerSettingsError` (`settings.ts`): the generated wire error (`zc_settings::errors`
/// computes its message).
pub use zc_contracts::ServerSettingsError;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_and_placeholders_are_transparent() {
        let id = ThreadId::new("thread-1");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"thread-1\"");
        let session = ProviderSession(json!({"threadId": "thread-1", "status": "ready"}));
        assert_eq!(serde_json::to_value(&session).unwrap(), json!({"threadId": "thread-1", "status": "ready"}));
    }

    #[test]
    fn background_scope_matches_the_wire() {
        let scopes = vec![
            BackgroundScope::ServerConfig,
            BackgroundScope::ProviderStatus { instance_id: None },
            BackgroundScope::ProviderStatus {
                instance_id: Some("codex".into()),
            },
            BackgroundScope::VcsStatus { cwd: "/r".into() },
            BackgroundScope::GitRefs { cwd: "/r".into() },
            BackgroundScope::Diagnostics,
            BackgroundScope::Thread { thread_id: "t".into() },
        ];
        assert_eq!(
            serde_json::to_value(&scopes).unwrap(),
            json!([
                {"type": "server-config"},
                {"type": "provider-status"},
                {"type": "provider-status", "instanceId": "codex"},
                {"type": "vcs-status", "cwd": "/r"},
                {"type": "git-refs", "cwd": "/r"},
                {"type": "diagnostics"},
                {"type": "thread", "threadId": "t"}
            ])
        );
        assert_eq!(scopes[2].key(), "provider-status:codex");
        assert_eq!(scopes[6].key(), "thread:t");
        assert_eq!(serde_json::to_value(WorktreeSubmodules::TopLevel).unwrap(), json!("top-level"));
    }

    #[test]
    fn tagged_errors_encode_tag_and_fields_only() {
        let error = TaggedError::new("TextGenerationError", "Text generation failed in branch: timeout")
            .with("operation", "branch")
            .with("detail", "timeout");
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({"_tag": "TextGenerationError", "operation": "branch", "detail": "timeout"})
        );
        assert_eq!(error.to_string(), "Text generation failed in branch: timeout");
        let decoded: TaggedError = serde_json::from_value(json!({"_tag": "X", "a": 1})).unwrap();
        assert!(decoded.is("X"));
        assert_eq!(decoded.fields["a"], json!(1));
    }
}
