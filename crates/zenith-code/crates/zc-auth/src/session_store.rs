//! `SessionStore` (`auth/SessionStore.ts`): signed session tokens and websocket tickets, the
//! `auth_sessions` rows behind them, the in-memory "connected" refcount, and the change feed of
//! `subscribeAuthAccess`.
//!
//! Verification order (what each failure is reported as depends on it):
//! 1. split into payload and signature (`MalformedSessionTokenError`);
//! 2. constant-time HMAC check (`InvalidSessionTokenSignatureError`);
//! 3. strict claims decode (`InvalidSessionTokenPayloadError`);
//! 4. `exp` must be a valid date (`InvalidSessionExpirationClaimError`), then `exp > now`
//!    (`SessionTokenExpiredError`);
//! 5. the row must exist (`UnknownSessionTokenError`) and not be revoked
//!    (`SessionTokenRevokedError`).
//!
//! The session's subject, scopes and method come from the signed claims; only the client
//! metadata comes from the row. Websocket tickets also check the row's expiry, and take
//! everything but the id from the row.

use std::collections::HashMap;
use std::sync::Mutex;

use jiff::Timestamp;
use zc_contracts::{
    AuthClientMetadata, AuthClientMetadataDeviceType as DeviceType, AuthClientSession, AuthEnvironmentScope as Scope, AuthSessionId, DateTimeUtc,
    ServerAuthSessionMethod as SessionMethod,
};
use zc_core::{PubSub, ServerSecretStore, Subscription};
use zc_db::repos::auth_sessions::{self, AuthSessionRecord, ClientMetadata, CreateAuthSession};
use zc_db::Db;

use crate::clock::SharedClock;
use crate::cookies::{resolve_legacy_session_cookie_name, resolve_session_cookie_name, CookieNameInput};
use crate::error::SessionCredentialError as E;
use crate::scopes::{parse_scopes, scope_strings, STANDARD_CLIENT_SCOPES};
use crate::token::{claim_millis, decode_payload_text, sign_claims, sign_payload, split_token, timing_safe_equal_base64url, SessionClaims, WebSocketClaims};

/// `server-signing-key`: 32 random bytes in `secrets/server-signing-key.bin`.
pub const SIGNING_SECRET_NAME: &str = "server-signing-key";
/// 30 days.
pub const DEFAULT_SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// 5 minutes.
pub const DEFAULT_WEBSOCKET_TOKEN_TTL_MS: i64 = 5 * 60 * 1000;
/// Reusable dev tokens are not ported, but a ticket naming one is still refused.
pub const REUSABLE_DEV_SESSION_PREFIX: &str = "dev-auth-";

/// `SessionStore.issue` input.
#[derive(Clone, Debug, Default)]
pub struct IssueSessionInput {
    pub ttl_ms: Option<i64>,
    /// Default `browser`.
    pub subject: Option<String>,
    /// Default `browser-session-cookie`.
    pub method: Option<SessionMethod>,
    /// Default the standard client scopes.
    pub scopes: Option<Vec<Scope>>,
    /// Default `{deviceType: "unknown"}`.
    pub client: Option<AuthClientMetadata>,
    pub proof_key_thumbprint: Option<String>,
    /// Atomically revoke the subject's live sessions of the same method first.
    pub replace_active_for_subject_and_method: bool,
}

/// `IssuedSession`.
#[derive(Clone, Debug, PartialEq)]
pub struct IssuedSession {
    pub session_id: String,
    pub token: String,
    pub method: SessionMethod,
    pub client: AuthClientMetadata,
    pub expires_at: i64,
    pub scopes: Vec<Scope>,
    pub proof_key_thumbprint: Option<String>,
}

/// `VerifiedSession`.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedSession {
    pub session_id: String,
    pub token: String,
    pub method: SessionMethod,
    pub client: AuthClientMetadata,
    pub expires_at: Option<i64>,
    pub subject: String,
    pub scopes: Vec<Scope>,
    pub proof_key_thumbprint: Option<String>,
}

/// `SessionCredentialChange`.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionCredentialChange {
    ClientUpserted(Box<AuthClientSession>),
    ClientRemoved(String),
}

/// A wire date from epoch milliseconds (always within the `Date` range here).
pub(crate) fn wire_date(millis: i64) -> DateTimeUtc {
    DateTimeUtc::from_millis(millis)
        .unwrap_or_else(|_| DateTimeUtc::from_millis(millis.clamp(-8_640_000_000_000_000, 8_640_000_000_000_000)).expect("clamped into range"))
}

pub(crate) fn timestamp(millis: i64) -> Result<Timestamp, String> {
    Timestamp::from_millisecond(millis).map_err(|e| e.to_string())
}

pub(crate) fn device_type_from_str(value: &str) -> DeviceType {
    DeviceType::ALL.iter().copied().find(|d| d.as_str() == value).unwrap_or(DeviceType::Unknown)
}

/// `toClientMetadata`: empty strings are dropped like `null`.
pub(crate) fn to_client_metadata(record: &ClientMetadata) -> AuthClientMetadata {
    let keep = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
    AuthClientMetadata {
        label: keep(&record.label),
        ip_address: keep(&record.ip_address),
        user_agent: keep(&record.user_agent),
        device_type: device_type_from_str(&record.device_type),
        os: keep(&record.os),
        browser: keep(&record.browser),
    }
}

fn to_record_client(client: &AuthClientMetadata) -> ClientMetadata {
    ClientMetadata {
        label: client.label.clone(),
        ip_address: client.ip_address.clone(),
        user_agent: client.user_agent.clone(),
        device_type: client.device_type.as_str().to_owned(),
        os: client.os.clone(),
        browser: client.browser.clone(),
    }
}

fn method_from_str(value: &str) -> Option<SessionMethod> {
    SessionMethod::ALL.iter().copied().find(|m| m.as_str() == value)
}

/// A row as the client-session view (`current` is always false here).
fn to_client_session(row: &AuthSessionRecord, connected: bool) -> Result<AuthClientSession, String> {
    Ok(AuthClientSession {
        session_id: AuthSessionId::new(row.session_id.clone()),
        subject: row.subject.clone(),
        scopes: parse_scopes(&row.scopes).ok_or("scopes: unknown scope")?,
        method: method_from_str(&row.method).ok_or("method: unknown method")?,
        client: to_client_metadata(&row.client),
        issued_at: wire_date(row.issued_at.as_millisecond()),
        expires_at: wire_date(row.expires_at.as_millisecond()),
        last_connected_at: row.last_connected_at.map(|t| wire_date(t.as_millisecond())),
        connected,
        current: false,
    })
}

/// The `SessionStore` service.
pub struct SessionStore {
    db: Db,
    clock: SharedClock,
    signing_key: Vec<u8>,
    cookie_name: String,
    legacy_cookie_name: Option<String>,
    connected: Mutex<HashMap<String, usize>>,
    changes: PubSub<SessionCredentialChange>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore").field("cookie_name", &self.cookie_name).finish_non_exhaustive()
    }
}

impl SessionStore {
    /// Reads (or creates) the signing key and derives the cookie names.
    pub async fn open(db: Db, secrets: &ServerSecretStore, cookie: &CookieNameInput, clock: SharedClock) -> Result<Self, zc_core::secrets::SecretStoreError> {
        let signing_key = secrets.get_or_create_random(SIGNING_SECRET_NAME, 32).await?;
        Ok(Self::with_signing_key(db, signing_key, cookie, clock))
    }

    pub fn with_signing_key(db: Db, signing_key: Vec<u8>, cookie: &CookieNameInput, clock: SharedClock) -> Self {
        Self {
            db,
            clock,
            signing_key,
            cookie_name: resolve_session_cookie_name(cookie),
            legacy_cookie_name: resolve_legacy_session_cookie_name(cookie),
            connected: Mutex::new(HashMap::new()),
            changes: PubSub::new(),
        }
    }

    pub fn cookie_name(&self) -> &str {
        &self.cookie_name
    }

    pub fn legacy_cookie_name(&self) -> Option<&str> {
        self.legacy_cookie_name.as_deref()
    }

    pub fn now(&self) -> i64 {
        self.clock.now_millis()
    }

    /// `streamChanges`: every change from now on.
    pub fn subscribe_changes(&self) -> Subscription<SessionCredentialChange> {
        self.changes.subscribe()
    }

    fn connected_map(&self) -> std::sync::MutexGuard<'_, HashMap<String, usize>> {
        self.connected.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The ids of the sessions with an open socket.
    pub fn connected_session_ids(&self) -> Vec<String> {
        self.connected_map().keys().cloned().collect()
    }

    fn emit_upsert(&self, session: AuthClientSession) {
        self.changes.publish(SessionCredentialChange::ClientUpserted(Box::new(session)));
    }

    fn emit_removed(&self, session_id: String) {
        self.changes.publish(SessionCredentialChange::ClientRemoved(session_id));
    }

    /// `issue`.
    pub async fn issue(&self, input: IssueSessionInput) -> Result<IssuedSession, E> {
        let session_id = zc_core::uuid_v4();
        let issued_at = self.now();
        let expires_at = issued_at + input.ttl_ms.unwrap_or(DEFAULT_SESSION_TTL_MS);
        let claims = SessionClaims {
            sid: session_id.clone(),
            sub: input.subject.unwrap_or_else(|| "browser".to_owned()),
            scopes: input.scopes.unwrap_or_else(|| STANDARD_CLIENT_SCOPES.to_vec()),
            method: input.method.unwrap_or(SessionMethod::BrowserSessionCookie),
            jkt: input.proof_key_thumbprint.filter(|j| !j.is_empty()),
            iat: issued_at as f64,
            exp: expires_at as f64,
        };
        let token = sign_claims(&claims.to_json(), &self.signing_key);
        let client = input.client.unwrap_or(AuthClientMetadata {
            label: None,
            ip_address: None,
            user_agent: None,
            device_type: DeviceType::Unknown,
            os: None,
            browser: None,
        });
        let issue_error = |cause: String| E::SessionCredentialIssue {
            session_id: Some(session_id.clone()),
            cause,
        };
        let record = CreateAuthSession {
            session_id: session_id.clone(),
            subject: claims.sub.clone(),
            scopes: scope_strings(&claims.scopes),
            method: claims.method.as_str().to_owned(),
            client: to_record_client(&client),
            issued_at: timestamp(issued_at).map_err(issue_error)?,
            expires_at: timestamp(expires_at).map_err(issue_error)?,
        };
        let replace = input.replace_active_for_subject_and_method;
        let revoked_at = record.issued_at;
        let replaced = self
            .db
            .call(move |conn| {
                if replace {
                    auth_sessions::create_replacing_active(conn, &record, revoked_at)
                } else {
                    auth_sessions::create(conn, &record).map(|()| Vec::new())
                }
            })
            .await
            .map_err(|e| issue_error(e.to_string()))?;
        if !replaced.is_empty() {
            {
                let mut connected = self.connected_map();
                for id in &replaced {
                    connected.remove(id);
                }
            }
            for id in replaced {
                self.emit_removed(id);
            }
        }
        self.emit_upsert(AuthClientSession {
            session_id: AuthSessionId::new(session_id.clone()),
            subject: claims.sub.clone(),
            scopes: claims.scopes.clone(),
            method: claims.method,
            client: client.clone(),
            issued_at: wire_date(issued_at),
            expires_at: wire_date(expires_at),
            last_connected_at: None,
            connected: false,
            current: false,
        });
        Ok(IssuedSession {
            session_id,
            token,
            method: claims.method,
            client,
            expires_at,
            scopes: claims.scopes,
            proof_key_thumbprint: claims.jkt,
        })
    }

    async fn get_row(&self, session_id: &str) -> Result<Option<AuthSessionRecord>, String> {
        let id = session_id.to_owned();
        self.db.call(move |conn| auth_sessions::get_by_id(conn, &id)).await.map_err(|e| e.to_string())
    }

    /// `verify`: a session token (cookie, bearer or DPoP access token).
    pub async fn verify(&self, token: &str) -> Result<VerifiedSession, E> {
        let (payload, signature) = split_token(token).ok_or(E::MalformedSessionToken)?;
        let expected = sign_payload(payload, &self.signing_key);
        if !timing_safe_equal_base64url(signature, &expected) {
            return Err(E::InvalidSessionTokenSignature);
        }
        let text = decode_payload_text(payload).ok_or_else(|| E::InvalidSessionTokenPayload {
            cause: "Invalid base64url payload".into(),
        })?;
        let claims = SessionClaims::from_json(&text).map_err(|cause| E::InvalidSessionTokenPayload { cause })?;
        let observed_at = self.now();
        let expires_at = claim_millis(claims.exp).ok_or_else(|| E::InvalidSessionExpirationClaim {
            session_id: claims.sid.clone(),
            expiration_claim: claims.exp,
        })?;
        if claims.exp <= observed_at as f64 {
            return Err(E::SessionTokenExpired {
                session_id: claims.sid,
                expires_at,
                observed_at,
            });
        }
        let row = self
            .get_row(&claims.sid)
            .await
            .map_err(|cause| E::SessionCredentialVerification {
                session_id: claims.sid.clone(),
                cause,
            })?
            .ok_or_else(|| E::UnknownSessionToken {
                session_id: claims.sid.clone(),
            })?;
        if let Some(revoked_at) = row.revoked_at {
            return Err(E::SessionTokenRevoked {
                session_id: claims.sid,
                revoked_at: revoked_at.as_millisecond(),
            });
        }
        Ok(VerifiedSession {
            session_id: claims.sid,
            token: token.to_owned(),
            method: claims.method,
            client: to_client_metadata(&row.client),
            expires_at: Some(expires_at),
            subject: claims.sub,
            scopes: claims.scopes,
            proof_key_thumbprint: claims.jkt.filter(|j| !j.is_empty()),
        })
    }

    /// `issueWebSocketToken`: a stateless ticket for `?wsTicket=`, reusable within its window.
    pub fn issue_websocket_token(&self, session_id: &str, ttl_ms: Option<i64>) -> (String, i64) {
        let issued_at = self.now();
        let expires_at = issued_at + ttl_ms.unwrap_or(DEFAULT_WEBSOCKET_TOKEN_TTL_MS);
        let claims = WebSocketClaims {
            sid: session_id.to_owned(),
            iat: issued_at as f64,
            exp: expires_at as f64,
        };
        (sign_claims(&claims.to_json(), &self.signing_key), expires_at)
    }

    /// `verifyWebSocketToken`.
    pub async fn verify_websocket_token(&self, token: &str) -> Result<VerifiedSession, E> {
        let (payload, signature) = split_token(token).ok_or(E::MalformedWebSocketToken)?;
        let expected = sign_payload(payload, &self.signing_key);
        if !timing_safe_equal_base64url(signature, &expected) {
            return Err(E::InvalidWebSocketTokenSignature);
        }
        let text = decode_payload_text(payload).ok_or_else(|| E::InvalidWebSocketTokenPayload {
            cause: "Invalid base64url payload".into(),
        })?;
        let claims = WebSocketClaims::from_json(&text).map_err(|cause| E::InvalidWebSocketTokenPayload { cause })?;
        if claims.sid.starts_with(REUSABLE_DEV_SESSION_PREFIX) {
            return Err(E::UnknownWebSocketSession { session_id: claims.sid });
        }
        let observed_at = self.now();
        let expires_at = claim_millis(claims.exp).ok_or_else(|| E::InvalidSessionExpirationClaim {
            session_id: claims.sid.clone(),
            expiration_claim: claims.exp,
        })?;
        if claims.exp <= observed_at as f64 {
            return Err(E::WebSocketTokenExpired {
                session_id: claims.sid,
                expires_at,
                observed_at,
            });
        }
        let verification = |cause: String| E::WebSocketTokenVerification {
            session_id: claims.sid.clone(),
            cause,
        };
        let row = self
            .get_row(&claims.sid)
            .await
            .map_err(verification)?
            .ok_or_else(|| E::UnknownWebSocketSession {
                session_id: claims.sid.clone(),
            })?;
        if row.expires_at.as_millisecond() <= observed_at {
            return Err(E::WebSocketSessionExpired {
                session_id: claims.sid,
                expires_at: row.expires_at.as_millisecond(),
                observed_at,
            });
        }
        if let Some(revoked_at) = row.revoked_at {
            return Err(E::WebSocketSessionRevoked {
                session_id: claims.sid,
                revoked_at: revoked_at.as_millisecond(),
            });
        }
        Ok(VerifiedSession {
            session_id: row.session_id.clone(),
            token: token.to_owned(),
            method: method_from_str(&row.method).ok_or_else(|| verification("method: unknown method".into()))?,
            client: to_client_metadata(&row.client),
            expires_at: Some(row.expires_at.as_millisecond()),
            subject: row.subject.clone(),
            scopes: parse_scopes(&row.scopes).ok_or_else(|| verification("scopes: unknown scope".into()))?,
            proof_key_thumbprint: None,
        })
    }

    /// `listActive`: live sessions, plus expired ones that still have a socket open; newest
    /// first.
    pub async fn list_active(&self) -> Result<Vec<AuthClientSession>, E> {
        let now = timestamp(self.now()).map_err(|cause| E::ActiveSessionsList { cause })?;
        let connected = self.connected_session_ids();
        let ids = connected.clone();
        let rows = self
            .db
            .call(move |conn| auth_sessions::list_active(conn, now, &ids))
            .await
            .map_err(|e| E::ActiveSessionsList { cause: e.to_string() })?;
        let connected = self.connected_map().clone();
        rows.iter()
            .map(|row| to_client_session(row, connected.contains_key(&row.session_id)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|cause| E::ActiveSessionsList { cause })
    }

    async fn load_active_session(&self, session_id: &str) -> Result<Option<AuthClientSession>, String> {
        let Some(row) = self.get_row(session_id).await? else {
            return Ok(None);
        };
        if row.revoked_at.is_some() {
            return Ok(None);
        }
        let connected = self.connected_map().contains_key(&row.session_id);
        if !connected && row.expires_at.as_millisecond() <= self.now() {
            return Ok(None);
        }
        to_client_session(&row, connected).map(Some)
    }

    /// `markConnected`: a socket opened. The first one records `last_connected_at`.
    pub async fn mark_connected(&self, session_id: &str) {
        let was_disconnected = {
            let mut connected = self.connected_map();
            let count = connected.entry(session_id.to_owned()).or_insert(0);
            *count += 1;
            *count == 1
        };
        let result: Result<(), String> = async {
            if was_disconnected {
                let at = timestamp(self.now())?;
                let id = session_id.to_owned();
                self.db
                    .call(move |conn| auth_sessions::set_last_connected_at(conn, &id, at))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            if let Some(session) = self.load_active_session(session_id).await? {
                self.emit_upsert(session);
            }
            Ok(())
        }
        .await;
        if let Err(cause) = result {
            tracing::error!(session_id, %cause, "Failed to publish connected-session auth update.");
        }
    }

    /// `markDisconnected`: a socket closed.
    pub async fn mark_disconnected(&self, session_id: &str) {
        {
            let mut connected = self.connected_map();
            let remaining = connected.get(session_id).copied().unwrap_or(0).saturating_sub(1);
            if remaining > 0 {
                connected.insert(session_id.to_owned(), remaining);
            } else {
                connected.remove(session_id);
            }
        }
        match self.load_active_session(session_id).await {
            Ok(Some(session)) => self.emit_upsert(session),
            Ok(None) => self.emit_removed(session_id.to_owned()),
            Err(cause) => {
                tracing::error!(session_id, %cause, "Failed to publish disconnected-session auth update.")
            }
        }
    }

    /// `recordClientConnection`: best effort, never fails a connect.
    pub async fn record_client_connection(&self, session_id: &str, surface: Option<&str>, app_version: Option<&str>) {
        if surface.is_none() && app_version.is_none() {
            return;
        }
        let id = session_id.to_owned();
        let surface = surface.map(str::to_owned);
        let app_version = app_version.map(str::to_owned);
        let result = self
            .db
            .call(move |conn| auth_sessions::set_client_connection(conn, &id, surface.as_deref(), app_version.as_deref()))
            .await;
        if let Err(cause) = result {
            tracing::warn!(session_id, %cause, "Failed to record session client connection metadata.");
        }
    }

    /// `revoke`.
    pub async fn revoke(&self, session_id: &str) -> Result<bool, E> {
        let at = timestamp(self.now()).map_err(|cause| E::SessionRevocation {
            session_id: session_id.to_owned(),
            cause,
        })?;
        let id = session_id.to_owned();
        let revoked = self
            .db
            .call(move |conn| auth_sessions::revoke(conn, &id, at))
            .await
            .map_err(|e| E::SessionRevocation {
                session_id: session_id.to_owned(),
                cause: e.to_string(),
            })?;
        if revoked {
            self.connected_map().remove(session_id);
            self.emit_removed(session_id.to_owned());
        }
        Ok(revoked)
    }

    /// `revokeAllExcept`: returns how many were revoked.
    pub async fn revoke_all_except(&self, session_id: &str) -> Result<usize, E> {
        let error = |cause: String| E::OtherSessionsRevocation {
            current_session_id: session_id.to_owned(),
            cause,
        };
        let at = timestamp(self.now()).map_err(error)?;
        let id = session_id.to_owned();
        let revoked = self
            .db
            .call(move |conn| auth_sessions::revoke_all_except(conn, &id, at))
            .await
            .map_err(|e| error(e.to_string()))?;
        {
            let mut connected = self.connected_map();
            for id in &revoked {
                connected.remove(id);
            }
        }
        let count = revoked.len();
        for id in revoked {
            self.emit_removed(id);
        }
        Ok(count)
    }
}
