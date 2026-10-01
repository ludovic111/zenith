//! The tagged errors of `SessionStore.ts`, `PairingGrantStore.ts` and `EnvironmentAuth.ts`.
//! Tags and messages are the TS ones (they show up in logs and CLI errors); only the HTTP
//! mapping in [`ServerAuthError`] reaches clients.

use zc_contracts::DpopFailureReason;

/// `SessionCredentialError` (invalid ∪ internal).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum SessionCredentialError {
    #[error("Malformed session token.")]
    MalformedSessionToken,
    #[error("Invalid session token signature.")]
    InvalidSessionTokenSignature,
    #[error("Invalid session token payload.")]
    InvalidSessionTokenPayload { cause: String },
    #[error("Session token expired.")]
    SessionTokenExpired { session_id: String, expires_at: i64, observed_at: i64 },
    #[error("Unknown session token.")]
    UnknownSessionToken { session_id: String },
    #[error("Session token revoked.")]
    SessionTokenRevoked { session_id: String, revoked_at: i64 },
    #[error("Invalid `exp` claim")]
    InvalidSessionExpirationClaim { session_id: String, expiration_claim: f64 },
    #[error("Malformed websocket token.")]
    MalformedWebSocketToken,
    #[error("Invalid websocket token signature.")]
    InvalidWebSocketTokenSignature,
    #[error("Invalid websocket token payload.")]
    InvalidWebSocketTokenPayload { cause: String },
    #[error("Websocket token expired.")]
    WebSocketTokenExpired { session_id: String, expires_at: i64, observed_at: i64 },
    #[error("Unknown websocket session.")]
    UnknownWebSocketSession { session_id: String },
    #[error("Websocket session expired.")]
    WebSocketSessionExpired { session_id: String, expires_at: i64, observed_at: i64 },
    #[error("Websocket session revoked.")]
    WebSocketSessionRevoked { session_id: String, revoked_at: i64 },
    // Internal errors.
    #[error("Failed to issue session credential.")]
    SessionCredentialIssue { session_id: Option<String>, cause: String },
    #[error("Failed to verify session credential.")]
    SessionCredentialVerification { session_id: String, cause: String },
    #[error("Failed to issue websocket token.")]
    WebSocketTokenIssue { session_id: String, cause: String },
    #[error("Failed to verify websocket token.")]
    WebSocketTokenVerification { session_id: String, cause: String },
    #[error("Failed to list active sessions.")]
    ActiveSessionsList { cause: String },
    #[error("Failed to revoke session.")]
    SessionRevocation { session_id: String, cause: String },
    #[error("Failed to revoke other sessions.")]
    OtherSessionsRevocation { current_session_id: String, cause: String },
}

impl SessionCredentialError {
    /// The TS `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::MalformedSessionToken => "MalformedSessionTokenError",
            Self::InvalidSessionTokenSignature => "InvalidSessionTokenSignatureError",
            Self::InvalidSessionTokenPayload { .. } => "InvalidSessionTokenPayloadError",
            Self::SessionTokenExpired { .. } => "SessionTokenExpiredError",
            Self::UnknownSessionToken { .. } => "UnknownSessionTokenError",
            Self::SessionTokenRevoked { .. } => "SessionTokenRevokedError",
            Self::InvalidSessionExpirationClaim { .. } => "InvalidSessionExpirationClaimError",
            Self::MalformedWebSocketToken => "MalformedWebSocketTokenError",
            Self::InvalidWebSocketTokenSignature => "InvalidWebSocketTokenSignatureError",
            Self::InvalidWebSocketTokenPayload { .. } => "InvalidWebSocketTokenPayloadError",
            Self::WebSocketTokenExpired { .. } => "WebSocketTokenExpiredError",
            Self::UnknownWebSocketSession { .. } => "UnknownWebSocketSessionError",
            Self::WebSocketSessionExpired { .. } => "WebSocketSessionExpiredError",
            Self::WebSocketSessionRevoked { .. } => "WebSocketSessionRevokedError",
            Self::SessionCredentialIssue { .. } => "SessionCredentialIssueError",
            Self::SessionCredentialVerification { .. } => "SessionCredentialVerificationError",
            Self::WebSocketTokenIssue { .. } => "WebSocketTokenIssueError",
            Self::WebSocketTokenVerification { .. } => "WebSocketTokenVerificationError",
            Self::ActiveSessionsList { .. } => "ActiveSessionsListError",
            Self::SessionRevocation { .. } => "SessionRevocationError",
            Self::OtherSessionsRevocation { .. } => "OtherSessionsRevocationError",
        }
    }

    /// `isSessionCredentialInvalidError`: the credential is bad (as opposed to the server
    /// failing to check it).
    pub fn is_invalid(&self) -> bool {
        !matches!(
            self,
            Self::SessionCredentialIssue { .. }
                | Self::SessionCredentialVerification { .. }
                | Self::WebSocketTokenIssue { .. }
                | Self::WebSocketTokenVerification { .. }
                | Self::ActiveSessionsList { .. }
                | Self::SessionRevocation { .. }
                | Self::OtherSessionsRevocation { .. }
        )
    }
}

/// `BootstrapCredentialError` (invalid ∪ internal).
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum BootstrapCredentialError {
    #[error("Unknown bootstrap credential.")]
    UnknownBootstrapCredential,
    #[error("Bootstrap credential expired.")]
    ExpiredBootstrapCredential,
    #[error("Bootstrap credential proof key mismatch.")]
    BootstrapCredentialProofKeyMismatch,
    #[error("Bootstrap credential is no longer available.")]
    UnavailableBootstrapCredential,
    // Internal errors.
    #[error("Failed to load active pairing links.")]
    ActivePairingLinksLoad { cause: String },
    #[error("Failed to revoke pairing link '{pairing_link_id}'.")]
    PairingLinkRevoke { pairing_link_id: String, cause: String },
    #[error("Failed to issue pairing credential '{pairing_link_id}' for '{subject}'.")]
    PairingCredentialIssue {
        pairing_link_id: String,
        subject: String,
        label: Option<String>,
        cause: String,
    },
    #[error("Failed to consume bootstrap credential.")]
    BootstrapCredentialConsume { cause: String },
    #[error("Failed to atomically consume an available bootstrap credential.")]
    BootstrapCredentialConsumeAvailable { cause: String },
    #[error("Failed to look up bootstrap credential state.")]
    BootstrapCredentialLookup { cause: String },
}

impl BootstrapCredentialError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::UnknownBootstrapCredential => "UnknownBootstrapCredentialError",
            Self::ExpiredBootstrapCredential => "ExpiredBootstrapCredentialError",
            Self::BootstrapCredentialProofKeyMismatch => "BootstrapCredentialProofKeyMismatchError",
            Self::UnavailableBootstrapCredential => "UnavailableBootstrapCredentialError",
            Self::ActivePairingLinksLoad { .. } => "ActivePairingLinksLoadError",
            Self::PairingLinkRevoke { .. } => "PairingLinkRevokeError",
            Self::PairingCredentialIssue { .. } => "PairingCredentialIssueError",
            Self::BootstrapCredentialConsume { .. } => "BootstrapCredentialConsumeError",
            Self::BootstrapCredentialConsumeAvailable { .. } => "BootstrapCredentialConsumeAvailableError",
            Self::BootstrapCredentialLookup { .. } => "BootstrapCredentialLookupError",
        }
    }

    /// `isBootstrapCredentialInternalError`.
    pub fn is_internal(&self) -> bool {
        !matches!(
            self,
            Self::UnknownBootstrapCredential
                | Self::ExpiredBootstrapCredential
                | Self::BootstrapCredentialProofKeyMismatch
                | Self::UnavailableBootstrapCredential
        )
    }
}

/// The errors of `EnvironmentAuth`, grouped the way the HTTP layer maps them.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ServerAuthError {
    /// `ServerAuthMissingCredentialError` → 401 `missing_credential`.
    #[error("Server authentication credential is missing.")]
    MissingCredential,
    /// `ServerAuthInvalidCredentialError` → 401 `invalid_credential` (+ `dpopFailureReason`).
    #[error("Server authentication credential is invalid.")]
    InvalidCredential {
        diagnostic: Option<String>,
        dpop_failure_reason: Option<DpopFailureReason>,
        cause: Option<String>,
    },
    /// `ServerAuthInvalidScopeError` → 400 `invalid_scope`.
    #[error("The requested authentication scope is invalid.")]
    InvalidScope,
    /// `ServerAuthScopeNotGrantedError` → 400 `scope_not_granted`.
    #[error("The requested authentication scope was not granted.")]
    ScopeNotGranted,
    /// `ServerAuthForbiddenOperationError` → 403.
    #[error("The current authentication session cannot revoke itself.")]
    ForbiddenOperation,
    /// One of the `ServerAuthInternalError` members → 500.
    #[error("{message}")]
    Internal { tag: &'static str, message: &'static str, cause: String },
}

impl ServerAuthError {
    pub fn invalid(cause: impl std::fmt::Display) -> Self {
        Self::InvalidCredential {
            diagnostic: None,
            dpop_failure_reason: None,
            cause: Some(cause.to_string()),
        }
    }

    pub fn dpop(diagnostic: impl Into<String>, reason: DpopFailureReason) -> Self {
        Self::InvalidCredential {
            diagnostic: Some(diagnostic.into()),
            dpop_failure_reason: Some(reason),
            cause: None,
        }
    }

    pub fn internal(tag: &'static str, message: &'static str, cause: impl std::fmt::Display) -> Self {
        Self::Internal {
            tag,
            message,
            cause: cause.to_string(),
        }
    }

    /// `isServerAuthCredentialError`.
    pub fn is_credential_error(&self) -> bool {
        matches!(self, Self::MissingCredential | Self::InvalidCredential { .. })
    }

    /// The TS `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::MissingCredential => "ServerAuthMissingCredentialError",
            Self::InvalidCredential { .. } => "ServerAuthInvalidCredentialError",
            Self::InvalidScope => "ServerAuthInvalidScopeError",
            Self::ScopeNotGranted => "ServerAuthScopeNotGrantedError",
            Self::ForbiddenOperation => "ServerAuthForbiddenOperationError",
            Self::Internal { tag, .. } => tag,
        }
    }

    /// The HTTP reason of a credential error (`serverAuthCredentialReason`).
    pub fn credential_reason(&self) -> &'static str {
        match self {
            Self::MissingCredential => "missing_credential",
            _ => "invalid_credential",
        }
    }

    pub fn dpop_failure_reason(&self) -> Option<DpopFailureReason> {
        match self {
            Self::InvalidCredential { dpop_failure_reason, .. } => *dpop_failure_reason,
            _ => None,
        }
    }
}

/// Internal error constructors, one per `ServerAuth*Error` tag used here.
pub(crate) mod internal {
    use super::ServerAuthError;

    macro_rules! internal_errors {
        ($($fn_name:ident => $tag:literal, $message:literal;)*) => {
            $(
                pub fn $fn_name(cause: impl std::fmt::Display) -> ServerAuthError {
                    ServerAuthError::internal($tag, $message, cause)
                }
            )*
        };
    }

    internal_errors! {
        bootstrap_credential_validation => "ServerAuthBootstrapCredentialValidationError", "Failed to validate bootstrap credential.";
        session_credential_validation => "ServerAuthSessionCredentialValidationError", "Failed to validate session credential.";
        authenticated_session_issue => "ServerAuthAuthenticatedSessionIssueError", "Failed to issue authenticated session.";
        authenticated_access_token_issue => "ServerAuthAuthenticatedAccessTokenIssueError", "Failed to issue authenticated access token.";
        pairing_link_creation => "ServerAuthPairingLinkCreationError", "Failed to create pairing link.";
        pairing_links_list => "ServerAuthPairingLinksListError", "Failed to list pairing links.";
        pairing_link_revocation => "ServerAuthPairingLinkRevocationError", "Failed to revoke pairing link.";
        session_token_issue => "ServerAuthSessionTokenIssueError", "Failed to issue session token.";
        sessions_list => "ServerAuthSessionsListError", "Failed to list sessions.";
        session_revocation => "ServerAuthSessionRevocationError", "Failed to revoke session.";
        other_sessions_revocation => "ServerAuthOtherSessionsRevocationError", "Failed to revoke other sessions.";
        dpop_replay_state_record => "ServerAuthDpopReplayStateRecordError", "Failed to record DPoP proof replay state.";
    }
}
