//! `EnvironmentAuth` (`auth/EnvironmentAuth.ts`): the auth control plane the HTTP API, the
//! `/ws` upgrade, the RPC handlers and the CLI share.
//!
//! Credential selection for a request (`selectRequestCredential`): the session cookie, then
//! `Authorization: Bearer`, then `Authorization: DPoP`, then the legacy `t3_session` cookie.
//! A token bound to a DPoP key (`jkt`) is only accepted as `Authorization: DPoP` with a valid
//! `DPoP` proof; a `DPoP` authorization with an unbound token is refused.

use std::net::SocketAddr;
use std::sync::Arc;

use http::HeaderMap;
use zc_contracts::{
    AuthAccessTokenResult, AuthAccessTokenResultTokenType, AuthBrowserSessionResult, AuthClientMetadata, AuthClientMetadataDeviceType as DeviceType,
    AuthClientSession, AuthEnvironmentScope as Scope, AuthPairingCredentialResult, AuthPairingLink, AuthSessionState, AuthWebSocketTicketResult,
    DpopFailureReason, JsNumber, LitTrue, LitUrnIetfParamsOauthTokenTypeAccessToken, ServerAuthBootstrapMethod, ServerAuthDescriptor,
    ServerAuthSessionMethod as SessionMethod,
};
use zc_core::ServerSecretStore;
use zc_db::Db;

use crate::clock::SharedClock;
use crate::cookies::{cookie_value, request_cookies, CookieNameInput};
use crate::dpop::{request_url, verify_request_dpop_proof};
use crate::error::{internal, BootstrapCredentialError, ServerAuthError, SessionCredentialError};
use crate::pairing::{IssueOneTimeTokenInput, PairingGrantStore};
use crate::policy::auth_descriptor;
use crate::scopes::{ADMINISTRATIVE_SCOPES, STANDARD_CLIENT_SCOPES};
use crate::session_store::{wire_date, IssueSessionInput, SessionStore, VerifiedSession};

/// The subject of `auth session issue` sessions.
pub const DEFAULT_SESSION_SUBJECT: &str = "cli-issued-session";
/// The subject of the startup credential printed in the `serve` banner (hidden from lists).
pub const INTERNAL_ADMINISTRATIVE_BOOTSTRAP_SUBJECT: &str = "administrative-bootstrap";
/// A DPoP-bound access token lives one hour.
pub const DPOP_ACCESS_TOKEN_TTL_MS: i64 = 60 * 60 * 1000;

const AUTHORIZATION_PREFIX: &str = "Bearer ";
const DPOP_AUTHORIZATION_PREFIX: &str = "DPoP ";

/// What auth needs to see of an HTTP request.
#[derive(Clone, Copy, Debug)]
pub struct AuthRequest<'a> {
    pub method: &'a str,
    pub headers: &'a HeaderMap,
    /// The request target as received (`/path?query`).
    pub path_and_query: &'a str,
    pub peer: Option<SocketAddr>,
}

/// A header as Node exposes it (first value, Latin-1 decoded).
pub fn header_text(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|value| value.as_bytes().iter().map(|b| char::from(*b)).collect())
}

impl AuthRequest<'_> {
    pub fn header(&self, name: &str) -> Option<String> {
        header_text(self.headers, name)
    }

    /// `HttpServerRequest.toURL`.
    pub fn url(&self) -> Option<String> {
        request_url(self.header("host").as_deref(), self.header("x-forwarded-proto").as_deref(), self.path_and_query)
    }

    pub fn remote_address(&self) -> Option<String> {
        self.peer.map(|peer| peer.ip().to_string())
    }
}

/// Where a request's credential came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSource {
    Cookie,
    Bearer,
    Dpop,
    LegacyCookie,
}

/// The selected credential of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedCredential {
    pub token: String,
    pub source: CredentialSource,
}

fn parse_authorization(headers: &HeaderMap, prefix: &str) -> Option<String> {
    let header = header_text(headers, "authorization")?;
    let token = crate::token::js_trim(header.strip_prefix(prefix)?);
    (!token.is_empty()).then(|| token.to_owned())
}

/// `selectRequestCredential`.
pub fn select_request_credential(headers: &HeaderMap, cookie_name: &str, legacy_cookie_name: Option<&str>) -> Option<SelectedCredential> {
    let cookies = request_cookies(headers);
    if let Some(token) = cookie_value(&cookies, cookie_name) {
        return Some(SelectedCredential {
            token: token.to_owned(),
            source: CredentialSource::Cookie,
        });
    }
    if let Some(token) = parse_authorization(headers, AUTHORIZATION_PREFIX) {
        return Some(SelectedCredential {
            token,
            source: CredentialSource::Bearer,
        });
    }
    if let Some(token) = parse_authorization(headers, DPOP_AUTHORIZATION_PREFIX) {
        return Some(SelectedCredential {
            token,
            source: CredentialSource::Dpop,
        });
    }
    let legacy = legacy_cookie_name.and_then(|name| cookie_value(&cookies, name))?;
    Some(SelectedCredential {
        token: legacy.to_owned(),
        source: CredentialSource::LegacyCookie,
    })
}

/// `AuthenticatedSession`.
#[derive(Clone, Debug, PartialEq)]
pub struct AuthenticatedSession {
    pub session_id: String,
    pub subject: String,
    pub method: SessionMethod,
    pub scopes: Vec<Scope>,
    pub proof_key_thumbprint: Option<String>,
    pub expires_at: Option<i64>,
}

impl AuthenticatedSession {
    pub fn has_scope(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }
}

impl From<VerifiedSession> for AuthenticatedSession {
    fn from(session: VerifiedSession) -> Self {
        Self {
            session_id: session.session_id,
            subject: session.subject,
            method: session.method,
            scopes: session.scopes,
            proof_key_thumbprint: session.proof_key_thumbprint,
            expires_at: session.expires_at,
        }
    }
}

/// `createBrowserSession`'s result: the body and the token to set as the cookie.
#[derive(Clone, Debug, PartialEq)]
pub struct BrowserSessionExchange {
    pub response: AuthBrowserSessionResult,
    pub session_token: String,
}

/// `createPairingLink` input.
#[derive(Clone, Debug, Default)]
pub struct CreatePairingLinkInput {
    pub ttl_ms: Option<i64>,
    pub label: Option<String>,
    pub scopes: Option<Vec<Scope>>,
    pub subject: Option<String>,
    pub proof_key_thumbprint: Option<String>,
    pub startup: bool,
}

/// `IssuedPairingLink`.
#[derive(Clone, Debug, PartialEq)]
pub struct IssuedPairingLink {
    pub id: String,
    pub credential: String,
    pub scopes: Vec<Scope>,
    pub subject: String,
    pub label: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// `issueSession` input.
#[derive(Clone, Debug, Default)]
pub struct IssueBearerSessionInput {
    pub ttl_ms: Option<i64>,
    pub subject: Option<String>,
    pub scopes: Option<Vec<Scope>>,
    pub label: Option<String>,
}

/// `IssuedBearerSession`.
#[derive(Clone, Debug, PartialEq)]
pub struct IssuedBearerSession {
    pub session_id: String,
    pub token: String,
    pub scopes: Vec<Scope>,
    pub subject: String,
    pub client: AuthClientMetadata,
    pub expires_at: i64,
}

/// `toBootstrapExchangeError`.
pub fn to_bootstrap_exchange_error(cause: BootstrapCredentialError) -> ServerAuthError {
    if cause.is_internal() {
        internal::bootstrap_credential_validation(format!("{}: {cause}", cause.tag()))
    } else {
        ServerAuthError::invalid(format!("{}: {cause}", cause.tag()))
    }
}

/// `mapSessionVerificationErrors`.
fn map_session_verification(cause: SessionCredentialError) -> ServerAuthError {
    if cause.is_invalid() {
        tracing::warn!(reason = %cause, "Rejected authenticated session credential.");
        ServerAuthError::invalid(format!("{}: {cause}", cause.tag()))
    } else {
        internal::session_credential_validation(format!("{}: {cause}", cause.tag()))
    }
}

/// `encodeOAuthScope`: space-separated, non-empty and unique.
fn encode_oauth_scope(scopes: &[Scope]) -> Option<String> {
    let mut seen = Vec::new();
    for scope in scopes {
        if seen.contains(scope) {
            return None;
        }
        seen.push(*scope);
    }
    (!scopes.is_empty()).then(|| scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" "))
}

/// `bySessionPriority`: managers first, then connected ones, then newest.
fn by_session_priority(left: &AuthClientSession, right: &AuthClientSession) -> std::cmp::Ordering {
    let left_manage = left.scopes.contains(&Scope::AccessWrite);
    let right_manage = right.scopes.contains(&Scope::AccessWrite);
    right_manage
        .cmp(&left_manage)
        .then(right.connected.cmp(&left.connected))
        .then(right.issued_at.as_millis().cmp(&left.issued_at.as_millis()))
}

/// The `EnvironmentAuth` service.
pub struct EnvironmentAuth {
    descriptor: ServerAuthDescriptor,
    sessions: Arc<SessionStore>,
    pairing: Arc<PairingGrantStore>,
    secrets: ServerSecretStore,
    clock: SharedClock,
}

impl std::fmt::Debug for EnvironmentAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvironmentAuth").field("descriptor", &self.descriptor).finish_non_exhaustive()
    }
}

impl EnvironmentAuth {
    /// Builds the auth services over an open database and secret store (the TS
    /// `EnvironmentAuth.layer`).
    pub async fn open(db: Db, secrets: ServerSecretStore, cookie: CookieNameInput, clock: SharedClock) -> Result<Self, zc_core::secrets::SecretStoreError> {
        let sessions = SessionStore::open(db.clone(), &secrets, &cookie, clock.clone()).await?;
        Ok(Self::from_parts(
            sessions,
            PairingGrantStore::new(db, clock.clone(), cookie.development),
            secrets,
            &cookie,
            clock,
        ))
    }

    pub fn from_parts(sessions: SessionStore, pairing: PairingGrantStore, secrets: ServerSecretStore, cookie: &CookieNameInput, clock: SharedClock) -> Self {
        Self {
            descriptor: auth_descriptor(cookie),
            sessions: Arc::new(sessions),
            pairing: Arc::new(pairing),
            secrets,
            clock,
        }
    }

    pub fn descriptor(&self) -> &ServerAuthDescriptor {
        &self.descriptor
    }

    pub fn sessions(&self) -> &Arc<SessionStore> {
        &self.sessions
    }

    pub fn pairing(&self) -> &Arc<PairingGrantStore> {
        &self.pairing
    }

    pub fn secrets(&self) -> &ServerSecretStore {
        &self.secrets
    }

    pub fn now(&self) -> i64 {
        self.clock.now_millis()
    }

    /// The request's credential, in selection order.
    pub fn select_credential(&self, headers: &HeaderMap) -> Option<SelectedCredential> {
        select_request_credential(headers, self.sessions.cookie_name(), self.sessions.legacy_cookie_name())
    }

    async fn authenticate_token(&self, token: &str) -> Result<AuthenticatedSession, ServerAuthError> {
        self.sessions
            .verify(token)
            .await
            .map(AuthenticatedSession::from)
            .map_err(map_session_verification)
    }

    /// `authenticateHttpRequest`.
    pub async fn authenticate_request(&self, request: &AuthRequest<'_>) -> Result<AuthenticatedSession, ServerAuthError> {
        let selected = self.select_credential(request.headers);
        let dpop_token = parse_authorization(request.headers, DPOP_AUTHORIZATION_PREFIX);
        let Some(credential) = selected.filter(|c| !c.token.is_empty()) else {
            return Err(ServerAuthError::MissingCredential);
        };
        let session = self.authenticate_token(&credential.token).await?;
        if let Some(thumbprint) = session.proof_key_thumbprint.as_deref() {
            if dpop_token.as_deref() != Some(credential.token.as_str()) {
                return Err(ServerAuthError::dpop(
                    "DPoP-bound access token requires DPoP authorization.",
                    DpopFailureReason::InvalidProof,
                ));
            }
            verify_request_dpop_proof(
                &self.secrets,
                request.header("dpop").as_deref(),
                request.method,
                request.url().as_deref(),
                self.now(),
                Some(thumbprint),
                dpop_token.as_deref(),
            )
            .await?;
            return Ok(session);
        }
        if dpop_token.is_some() {
            return Err(ServerAuthError::dpop(
                "DPoP authorization requires a proof-bound access token.",
                DpopFailureReason::InvalidProof,
            ));
        }
        Ok(session)
    }

    /// `authenticateWebSocketUpgrade`: `?wsTicket=` when present and non-blank, otherwise the
    /// request's credential.
    pub async fn authenticate_websocket_upgrade(&self, request: &AuthRequest<'_>, ws_ticket: Option<&str>) -> Result<AuthenticatedSession, ServerAuthError> {
        if let Some(ticket) = ws_ticket.filter(|t| !crate::token::js_trim(t).is_empty()) {
            return self
                .sessions
                .verify_websocket_token(ticket)
                .await
                .map(|session| AuthenticatedSession {
                    proof_key_thumbprint: None,
                    ..AuthenticatedSession::from(session)
                })
                .map_err(map_session_verification);
        }
        self.authenticate_request(request).await
    }

    /// `getSessionState`: never fails on a bad credential, only on internal errors.
    pub async fn get_session_state(&self, request: &AuthRequest<'_>) -> Result<AuthSessionState, ServerAuthError> {
        match self.authenticate_request(request).await {
            Ok(session) => Ok(AuthSessionState {
                authenticated: true,
                auth: self.descriptor.clone(),
                scopes: Some(session.scopes),
                session_method: Some(session.method),
                expires_at: session.expires_at.map(wire_date),
            }),
            Err(error) if error.is_credential_error() => Ok(AuthSessionState {
                authenticated: false,
                auth: self.descriptor.clone(),
                scopes: None,
                session_method: None,
                expires_at: None,
            }),
            Err(error) => Err(error),
        }
    }

    /// `createBrowserSession`: consumes a pairing credential and issues a cookie session.
    pub async fn create_browser_session(&self, credential: &str, request_metadata: AuthClientMetadata) -> Result<BrowserSessionExchange, ServerAuthError> {
        let grant = self.pairing.consume(credential, None).await.map_err(to_bootstrap_exchange_error)?;
        let mut client = request_metadata;
        if let Some(label) = grant.label.clone().filter(|l| !l.is_empty()) {
            client.label = Some(label);
        }
        let session = self
            .sessions
            .issue(IssueSessionInput {
                method: Some(SessionMethod::BrowserSessionCookie),
                subject: Some(grant.subject),
                scopes: Some(grant.scopes),
                client: Some(client),
                ..IssueSessionInput::default()
            })
            .await
            .map_err(internal::authenticated_session_issue)?;
        Ok(BrowserSessionExchange {
            response: AuthBrowserSessionResult {
                authenticated: LitTrue,
                scopes: session.scopes,
                session_method: session.method,
                expires_at: wire_date(session.expires_at),
            },
            session_token: session.token,
        })
    }

    /// `exchangeBootstrapCredentialForAccessToken` (`POST /oauth/token`).
    pub async fn exchange_bootstrap_credential_for_access_token(
        &self,
        credential: &str,
        requested_scopes: Option<Vec<Scope>>,
        request_metadata: AuthClientMetadata,
        proof_key_thumbprint: Option<String>,
    ) -> Result<AuthAccessTokenResult, ServerAuthError> {
        let proof_key_thumbprint = proof_key_thumbprint.filter(|t| !t.is_empty());
        let grant = self
            .pairing
            .consume(credential, proof_key_thumbprint.as_deref())
            .await
            .map_err(to_bootstrap_exchange_error)?;
        let granted = requested_scopes.unwrap_or_else(|| grant.scopes.clone());
        if !granted.iter().all(|scope| grant.scopes.contains(scope)) {
            return Err(ServerAuthError::ScopeNotGranted);
        }
        let mut client = request_metadata;
        if let Some(label) = grant.label.clone().filter(|l| !l.is_empty()) {
            client.label = Some(label);
        }
        let dpop = proof_key_thumbprint.is_some();
        let session = self
            .sessions
            .issue(IssueSessionInput {
                method: Some(if dpop {
                    SessionMethod::DpopAccessToken
                } else {
                    SessionMethod::BearerAccessToken
                }),
                subject: Some(grant.subject),
                scopes: Some(granted),
                ttl_ms: dpop.then_some(DPOP_ACCESS_TOKEN_TTL_MS),
                proof_key_thumbprint,
                // Desktop restarts forget the previous bearer token: replace its session.
                replace_active_for_subject_and_method: grant.method == ServerAuthBootstrapMethod::DesktopBootstrap,
                client: Some(client),
            })
            .await
            .map_err(internal::authenticated_access_token_issue)?;
        let now = self.now();
        let scope = encode_oauth_scope(&session.scopes)
            .ok_or_else(|| internal::authenticated_access_token_issue("OAuth scopes must be non-empty, syntactically valid, and unique."))?;
        Ok(AuthAccessTokenResult {
            access_token: session.token,
            issued_token_type: LitUrnIetfParamsOauthTokenTypeAccessToken,
            token_type: if dpop {
                AuthAccessTokenResultTokenType::DPoP
            } else {
                AuthAccessTokenResultTokenType::Bearer
            },
            expires_in: JsNumber(((session.expires_at - now).div_euclid(1000)).max(0) as f64),
            scope,
        })
    }

    /// `createPairingLink`.
    pub async fn create_pairing_link(&self, input: CreatePairingLinkInput) -> Result<IssuedPairingLink, ServerAuthError> {
        let created_at = self.now();
        let scopes = input.scopes.unwrap_or_else(|| STANDARD_CLIENT_SCOPES.to_vec());
        let subject = input.subject.unwrap_or_else(|| "one-time-token".to_owned());
        let issued = self
            .pairing
            .issue_one_time_token(IssueOneTimeTokenInput {
                ttl_ms: input.ttl_ms,
                scopes: Some(scopes.clone()),
                subject: Some(subject.clone()),
                label: input.label.filter(|l| !l.is_empty()),
                proof_key_thumbprint: input.proof_key_thumbprint,
                startup: input.startup,
            })
            .await
            .map_err(|cause| internal::pairing_link_creation(format!("{}: {cause}", cause.tag())))?;
        Ok(IssuedPairingLink {
            id: issued.id,
            credential: issued.credential,
            scopes,
            subject,
            label: issued.label,
            created_at,
            expires_at: issued.expires_at,
        })
    }

    fn pairing_credential_result(issued: IssuedPairingLink) -> AuthPairingCredentialResult {
        AuthPairingCredentialResult {
            id: issued.id,
            credential: issued.credential,
            label: issued.label,
            expires_at: wire_date(issued.expires_at),
        }
    }

    /// `issuePairingCredential` (`POST /api/auth/pairing-token`).
    pub async fn issue_pairing_credential(&self, label: Option<String>, scopes: Option<Vec<Scope>>) -> Result<AuthPairingCredentialResult, ServerAuthError> {
        self.create_pairing_link(CreatePairingLinkInput {
            scopes: Some(scopes.unwrap_or_else(|| STANDARD_CLIENT_SCOPES.to_vec())),
            subject: Some("one-time-token".into()),
            label,
            ..CreatePairingLinkInput::default()
        })
        .await
        .map(Self::pairing_credential_result)
    }

    /// `issueStartupPairingCredential`: the administrative one-time credential of the `serve`
    /// banner.
    pub async fn issue_startup_pairing_credential(&self) -> Result<AuthPairingCredentialResult, ServerAuthError> {
        self.create_pairing_link(CreatePairingLinkInput {
            scopes: Some(ADMINISTRATIVE_SCOPES.to_vec()),
            subject: Some(INTERNAL_ADMINISTRATIVE_BOOTSTRAP_SUBJECT.into()),
            startup: true,
            ..CreatePairingLinkInput::default()
        })
        .await
        .map(Self::pairing_credential_result)
    }

    /// `issueStartupPairingUrl`: `<base>/pair#token=<credential>`.
    pub async fn issue_startup_pairing_url(&self, base_url: &str) -> Result<String, ServerAuthError> {
        let issued = self.issue_startup_pairing_credential().await?;
        build_pairing_url(base_url, &issued.credential).ok_or_else(|| internal::pairing_link_creation(format!("Invalid URL: {base_url}")))
    }

    /// `listPairingLinks`: hides the startup credential unless other subjects are given.
    pub async fn list_pairing_links(&self, exclude_subjects: Option<&[&str]>) -> Result<Vec<AuthPairingLink>, ServerAuthError> {
        let excluded = exclude_subjects.unwrap_or(&[INTERNAL_ADMINISTRATIVE_BOOTSTRAP_SUBJECT]);
        let mut links = self
            .pairing
            .list_active()
            .await
            .map_err(|cause| internal::pairing_links_list(format!("{}: {cause}", cause.tag())))?;
        links.retain(|link| !excluded.contains(&link.subject.as_str()));
        links.sort_by_key(|link| std::cmp::Reverse(link.created_at.as_millis()));
        Ok(links)
    }

    /// `revokePairingLink`.
    pub async fn revoke_pairing_link(&self, id: &str) -> Result<bool, ServerAuthError> {
        self.pairing
            .revoke(id)
            .await
            .map_err(|cause| internal::pairing_link_revocation(format!("{}: {cause}", cause.tag())))
    }

    /// `issueSession` (`auth session issue`): a bearer session for a bot.
    pub async fn issue_session(&self, input: IssueBearerSessionInput) -> Result<IssuedBearerSession, ServerAuthError> {
        let subject = input.subject.unwrap_or_else(|| DEFAULT_SESSION_SUBJECT.to_owned());
        let issued = self
            .sessions
            .issue(IssueSessionInput {
                subject: Some(subject.clone()),
                method: Some(SessionMethod::BearerAccessToken),
                scopes: Some(input.scopes.unwrap_or_else(|| ADMINISTRATIVE_SCOPES.to_vec())),
                client: Some(AuthClientMetadata {
                    label: input.label.filter(|l| !l.is_empty()),
                    ip_address: None,
                    user_agent: None,
                    device_type: DeviceType::Bot,
                    os: None,
                    browser: None,
                }),
                ttl_ms: input.ttl_ms,
                ..IssueSessionInput::default()
            })
            .await
            .map_err(|cause| internal::session_token_issue(format!("{}: {cause}", cause.tag())))?;
        Ok(IssuedBearerSession {
            session_id: issued.session_id,
            token: issued.token,
            scopes: issued.scopes,
            subject,
            client: issued.client,
            expires_at: issued.expires_at,
        })
    }

    /// `listSessions`: sorted by priority.
    pub async fn list_sessions(&self) -> Result<Vec<AuthClientSession>, ServerAuthError> {
        let mut sessions = self
            .sessions
            .list_active()
            .await
            .map_err(|cause| internal::sessions_list(format!("{}: {cause}", cause.tag())))?;
        sessions.sort_by(by_session_priority);
        Ok(sessions)
    }

    /// `revokeSession`.
    pub async fn revoke_session(&self, session_id: &str) -> Result<bool, ServerAuthError> {
        self.sessions
            .revoke(session_id)
            .await
            .map_err(|cause| internal::session_revocation(format!("{}: {cause}", cause.tag())))
    }

    /// `revokeOtherSessionsExcept`.
    pub async fn revoke_other_sessions_except(&self, session_id: &str) -> Result<usize, ServerAuthError> {
        self.sessions
            .revoke_all_except(session_id)
            .await
            .map_err(|cause| internal::other_sessions_revocation(format!("{}: {cause}", cause.tag())))
    }

    /// `listClientSessions`: `current` marks the caller's own session.
    pub async fn list_client_sessions(&self, current_session_id: &str) -> Result<Vec<AuthClientSession>, ServerAuthError> {
        Ok(self
            .list_sessions()
            .await?
            .into_iter()
            .map(|mut session| {
                session.current = session.session_id.as_str() == current_session_id;
                session
            })
            .collect())
    }

    /// `revokeClientSession`: a session cannot revoke itself.
    pub async fn revoke_client_session(&self, current_session_id: &str, target_session_id: &str) -> Result<bool, ServerAuthError> {
        if current_session_id == target_session_id {
            return Err(ServerAuthError::ForbiddenOperation);
        }
        self.revoke_session(target_session_id).await
    }

    /// `revokeOtherClientSessions`.
    pub async fn revoke_other_client_sessions(&self, current_session_id: &str) -> Result<usize, ServerAuthError> {
        self.revoke_other_sessions_except(current_session_id).await
    }

    /// `issueWebSocketTicket`.
    pub fn issue_websocket_ticket(&self, session_id: &str) -> AuthWebSocketTicketResult {
        let (ticket, expires_at) = self.sessions.issue_websocket_token(session_id, None);
        AuthWebSocketTicketResult {
            ticket,
            expires_at: wire_date(expires_at),
        }
    }
}

/// `buildPairingUrl`: `/pair` with `#token=<credential>` (form-encoded), query `token` removed.
pub fn build_pairing_url(base_url: &str, credential: &str) -> Option<String> {
    let mut url = url::Url::parse(base_url).ok()?;
    url.set_path("/pair");
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "token")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if url.query().is_some() {
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
    }
    let fragment: String = url::form_urlencoded::Serializer::new(String::new()).append_pair("token", credential).finish();
    url.set_fragment(Some(&fragment));
    Some(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_credentials_in_order() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer bearer-token".parse().unwrap());
        headers.insert("cookie", "t3_session=legacy; other=x".parse().unwrap());
        // Bearer beats a stale legacy cookie (EnvironmentAuth.test.ts).
        let selected = select_request_credential(&headers, "t3_session_1_abc", Some("t3_session")).unwrap();
        assert_eq!(selected.source, CredentialSource::Bearer);
        assert_eq!(selected.token, "bearer-token");
        headers.insert("cookie", "t3_session_1_abc=cookie-token".parse().unwrap());
        let selected = select_request_credential(&headers, "t3_session_1_abc", Some("t3_session")).unwrap();
        assert_eq!(selected.source, CredentialSource::Cookie);
        headers.remove("cookie");
        headers.insert("authorization", "DPoP  dpop-token ".parse().unwrap());
        let selected = select_request_credential(&headers, "c", None).unwrap();
        assert_eq!((selected.source, selected.token.as_str()), (CredentialSource::Dpop, "dpop-token"));
        headers.insert("authorization", "Bearer   ".parse().unwrap());
        assert_eq!(select_request_credential(&headers, "c", None), None);
        headers.insert("authorization", "bearer x".parse().unwrap());
        assert_eq!(select_request_credential(&headers, "c", None), None);
    }

    #[test]
    fn builds_pairing_urls() {
        assert_eq!(
            build_pairing_url("http://127.0.0.1:3773", "ABC").unwrap(),
            "http://127.0.0.1:3773/pair#token=ABC"
        );
        assert_eq!(
            build_pairing_url("http://localhost:5173/app?token=old&x=1#h", "A B").unwrap(),
            "http://localhost:5173/pair?x=1#token=A+B"
        );
    }

    #[test]
    fn encodes_oauth_scopes() {
        assert_eq!(encode_oauth_scope(&[Scope::AccessRead]).as_deref(), Some("access:read"));
        assert_eq!(encode_oauth_scope(&[]), None);
        assert_eq!(encode_oauth_scope(&[Scope::AccessRead, Scope::AccessRead]), None);
    }
}
