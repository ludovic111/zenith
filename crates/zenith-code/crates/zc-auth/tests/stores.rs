//! SessionStore, PairingGrantStore and EnvironmentAuth, ported from `SessionStore.test.ts`,
//! `PairingGrantStore.test.ts`, `EnvironmentAuth.test.ts` and `EnvironmentAuthAdmin.test.ts`.
//! Each test gets an in-memory database with every migration and a settable clock.

use std::sync::Arc;

use futures::StreamExt;
use http::HeaderMap;
use zc_auth::environment_auth::{to_bootstrap_exchange_error, AuthRequest, CreatePairingLinkInput};
use zc_auth::pairing::{BootstrapCredentialChange, IssueOneTimeTokenInput};
use zc_auth::session_store::{IssueSessionInput, SessionCredentialChange};
use zc_auth::{
    BootstrapCredentialError, CookieNameInput, EnvironmentAuth, IssueBearerSessionInput, PairingGrantStore, ServerAuthError, ServerMode,
    SessionCredentialError, SessionStore, TestClock, ADMINISTRATIVE_SCOPES, STANDARD_CLIENT_SCOPES,
};
use zc_contracts::{AuthClientMetadata, AuthClientMetadataDeviceType as DeviceType, AuthEnvironmentScope as Scope, ServerAuthSessionMethod as Method};
use zc_core::ServerSecretStore;
use zc_db::{Db, DbError};

const START: i64 = 1_790_000_000_000;
const HOUR: i64 = 60 * 60 * 1000;

fn cookie_input(host: &str) -> CookieNameInput {
    CookieNameInput {
        mode: ServerMode::Web,
        port: 3773,
        host: Some(host.into()),
        instance_key: "/tmp/t3-auth-session-test".into(),
        environment_id: "test-environment".into(),
        development: false,
    }
}

fn session_store(db: &Db, clock: &Arc<TestClock>) -> SessionStore {
    SessionStore::with_signing_key(db.clone(), vec![7; 32], &cookie_input("127.0.0.1"), clock.clone())
}

fn setup() -> (Db, Arc<TestClock>, SessionStore) {
    let db = Db::open_in_memory().unwrap();
    let clock = TestClock::new(START);
    let store = session_store(&db, &clock);
    (db, clock, store)
}

fn client(label: &str, device: DeviceType) -> AuthClientMetadata {
    AuthClientMetadata {
        label: Some(label.into()),
        ip_address: None,
        user_agent: None,
        device_type: device,
        os: None,
        browser: None,
    }
}

fn relay_session_input() -> IssueSessionInput {
    IssueSessionInput {
        subject: Some("managed-relay-bootstrap".into()),
        method: Some(Method::DpopAccessToken),
        proof_key_thumbprint: Some("relay-proof-key".into()),
        ttl_ms: Some(HOUR),
        client: Some(client("Relay desktop", DeviceType::Desktop)),
        ..Default::default()
    }
}

async fn exec(db: &Db, sql: &'static str) {
    db.call(move |conn| conn.execute_batch(sql).map_err(|e| DbError::sql("test", e))).await.unwrap();
}

// SessionStore.test.ts

#[tokio::test]
async fn issues_and_verifies_signed_browser_session_tokens() {
    let (_db, _clock, sessions) = setup();
    let issued = sessions
        .issue(IssueSessionInput {
            subject: Some("desktop-bootstrap".into()),
            scopes: Some(vec![Scope::OrchestrationRead, Scope::AccessWrite]),
            client: Some(AuthClientMetadata {
                label: Some("Desktop app".into()),
                ip_address: Some("127.0.0.1".into()),
                user_agent: None,
                device_type: DeviceType::Desktop,
                os: Some("macOS".into()),
                browser: Some("Electron".into()),
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let verified = sessions.verify(&issued.token).await.unwrap();
    assert_eq!(verified.method, Method::BrowserSessionCookie);
    assert_eq!(verified.subject, "desktop-bootstrap");
    assert_eq!(verified.scopes, vec![Scope::OrchestrationRead, Scope::AccessWrite]);
    assert_eq!(verified.client.label.as_deref(), Some("Desktop app"));
    assert_eq!(verified.client.browser.as_deref(), Some("Electron"));
    assert_eq!(verified.expires_at, Some(issued.expires_at));
    assert_eq!(issued.expires_at, START + 30 * 24 * HOUR);
}

#[tokio::test]
async fn rejects_malformed_and_tampered_session_tokens() {
    let (_db, _clock, sessions) = setup();
    let error = sessions.verify("not-a-session-token").await.unwrap_err();
    assert_eq!(error.tag(), "MalformedSessionTokenError");
    assert!(error.to_string().contains("Malformed session token"));
    let issued = sessions.issue(IssueSessionInput::default()).await.unwrap();
    let (payload, signature) = issued.token.split_once('.').unwrap();
    let forged = format!("{payload}x.{signature}");
    assert_eq!(sessions.verify(&forged).await.unwrap_err().tag(), "InvalidSessionTokenSignatureError");
    // Another key signed it.
    let other = SessionStore::with_signing_key(Db::open_in_memory().unwrap(), vec![8; 32], &cookie_input("127.0.0.1"), TestClock::new(START));
    assert_eq!(other.verify(&issued.token).await.unwrap_err().tag(), "InvalidSessionTokenSignatureError");
    // Extra dot-separated parts are ignored, like `token.split(".")` in TS.
    assert!(sessions.verify(&format!("{}.extra", issued.token)).await.is_ok());
    // A websocket ticket is not a session token.
    let (ticket, _) = sessions.issue_websocket_token(&issued.session_id, None);
    assert_eq!(sessions.verify(&ticket).await.unwrap_err().tag(), "InvalidSessionTokenPayloadError");
}

#[tokio::test]
async fn preserves_repository_failures_while_verifying() {
    let (db, _clock, sessions) = setup();
    let issued = sessions
        .issue(IssueSessionInput {
            method: Some(Method::BearerAccessToken),
            subject: Some("repository-failure".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let (ticket, _) = sessions.issue_websocket_token(&issued.session_id, None);
    exec(&db, "DROP TABLE auth_sessions").await;
    let session_error = sessions.verify(&issued.token).await.unwrap_err();
    let ws_error = sessions.verify_websocket_token(&ticket).await.unwrap_err();
    assert!(matches!(&session_error, SessionCredentialError::SessionCredentialVerification { session_id, .. } if *session_id == issued.session_id));
    assert!(matches!(&ws_error, SessionCredentialError::WebSocketTokenVerification { session_id, .. } if *session_id == issued.session_id));
    assert!(!session_error.is_invalid());
    assert_eq!(sessions.revoke(&issued.session_id).await.unwrap_err().tag(), "SessionRevocationError");
    assert_eq!(
        sessions.revoke_all_except(&issued.session_id).await.unwrap_err().tag(),
        "OtherSessionsRevocationError"
    );
}

#[tokio::test]
async fn bearer_sessions_default_to_standard_scopes() {
    let (_db, _clock, sessions) = setup();
    let issued = sessions
        .issue(IssueSessionInput {
            method: Some(Method::BearerAccessToken),
            subject: Some("test-clock".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let verified = sessions.verify(&issued.token).await.unwrap();
    assert_eq!(verified.method, Method::BearerAccessToken);
    assert_eq!(verified.subject, "test-clock");
    assert_eq!(verified.scopes, STANDARD_CLIENT_SCOPES.to_vec());
}

#[tokio::test]
async fn atomically_replaces_active_sessions_with_the_same_subject_and_method() {
    let (_db, _clock, sessions) = setup();
    let sessions = Arc::new(sessions);
    let browser = sessions
        .issue(IssueSessionInput {
            subject: Some("desktop-bootstrap".into()),
            method: Some(Method::BrowserSessionCookie),
            ..Default::default()
        })
        .await
        .unwrap();
    let replace = || IssueSessionInput {
        subject: Some("desktop-bootstrap".into()),
        method: Some(Method::BearerAccessToken),
        replace_active_for_subject_and_method: true,
        ..Default::default()
    };
    let (first, second) = tokio::join!(sessions.issue(replace()), sessions.issue(replace()));
    let (first, second) = (first.unwrap(), second.unwrap());
    let active = sessions.list_active().await.unwrap();
    assert_eq!(active.len(), 2);
    assert!(active.iter().any(|s| s.session_id.as_str() == browser.session_id));
    assert_eq!(
        active
            .iter()
            .filter(|s| s.subject == "desktop-bootstrap" && s.method == Method::BearerAccessToken)
            .count(),
        1
    );
    let valid = [sessions.verify(&first.token).await.is_ok(), sessions.verify(&second.token).await.is_ok()];
    assert_eq!(valid.iter().filter(|v| **v).count(), 1);
}

#[tokio::test]
async fn keeps_the_previous_session_valid_when_replacement_fails() {
    let (db, _clock, sessions) = setup();
    let previous = sessions
        .issue(IssueSessionInput {
            subject: Some("desktop-bootstrap".into()),
            method: Some(Method::BearerAccessToken),
            ..Default::default()
        })
        .await
        .unwrap();
    exec(
        &db,
        "CREATE TRIGGER reject_auth_session_insert BEFORE INSERT ON auth_sessions
         BEGIN SELECT RAISE(ABORT, 'simulated insert failure'); END",
    )
    .await;
    let error = sessions
        .issue(IssueSessionInput {
            subject: Some("desktop-bootstrap".into()),
            method: Some(Method::BearerAccessToken),
            replace_active_for_subject_and_method: true,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "SessionCredentialIssueError");
    assert_eq!(sessions.verify(&previous.token).await.unwrap().session_id, previous.session_id);
    let listed: Vec<String> = sessions.list_active().await.unwrap().iter().map(|s| s.session_id.as_str().to_owned()).collect();
    assert_eq!(listed, vec![previous.session_id]);
}

#[tokio::test]
async fn rejects_websocket_tokens_once_the_parent_session_has_expired() {
    let (_db, clock, sessions) = setup();
    let issued = sessions
        .issue(IssueSessionInput {
            method: Some(Method::BearerAccessToken),
            subject: Some("short-lived".into()),
            ttl_ms: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap();
    let (ticket, _) = sessions.issue_websocket_token(&issued.session_id, None);
    clock.advance(2000);
    match sessions.verify_websocket_token(&ticket).await.unwrap_err() {
        SessionCredentialError::WebSocketSessionExpired {
            session_id,
            expires_at,
            observed_at,
        } => {
            assert_eq!(session_id, issued.session_id);
            assert_eq!(expires_at, issued.expires_at);
            assert!(observed_at > expires_at);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn includes_expiry_context_when_tokens_expire() {
    let (_db, clock, sessions) = setup();
    let issued = sessions
        .issue(IssueSessionInput {
            method: Some(Method::BearerAccessToken),
            subject: Some("short-lived-token".into()),
            ttl_ms: Some(1000),
            ..Default::default()
        })
        .await
        .unwrap();
    let (ticket, ticket_expires) = sessions.issue_websocket_token(&issued.session_id, Some(1000));
    clock.advance(2000);
    match sessions.verify(&issued.token).await.unwrap_err() {
        SessionCredentialError::SessionTokenExpired {
            session_id,
            expires_at,
            observed_at,
        } => {
            assert_eq!(session_id, issued.session_id);
            assert_eq!(expires_at, issued.expires_at);
            assert!(observed_at > expires_at);
        }
        other => panic!("{other:?}"),
    }
    match sessions.verify_websocket_token(&ticket).await.unwrap_err() {
        SessionCredentialError::WebSocketTokenExpired {
            session_id,
            expires_at,
            observed_at,
        } => {
            assert_eq!(session_id, issued.session_id);
            assert_eq!(expires_at, ticket_expires);
            assert!(observed_at > expires_at);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn lists_active_sessions_tracks_connectivity_and_revokes_others() {
    let (_db, _clock, sessions) = setup();
    let administrative = sessions
        .issue(IssueSessionInput {
            subject: Some("desktop-bootstrap".into()),
            scopes: Some(vec![Scope::OrchestrationRead, Scope::AccessWrite]),
            client: Some(client("Desktop app", DeviceType::Desktop)),
            ..Default::default()
        })
        .await
        .unwrap();
    let phone = sessions
        .issue(IssueSessionInput {
            subject: Some("one-time-token".into()),
            scopes: Some(vec![Scope::OrchestrationRead]),
            client: Some(AuthClientMetadata {
                ip_address: Some("192.168.1.88".into()),
                os: Some("iOS".into()),
                browser: Some("Safari".into()),
                ..client("Synthetic phone", DeviceType::Mobile)
            }),
            ..Default::default()
        })
        .await
        .unwrap();
    let (phone_ticket, _) = sessions.issue_websocket_token(&phone.session_id, None);
    sessions.mark_connected(&phone.session_id).await;
    let before = sessions.list_active().await.unwrap();
    let revoked = sessions.revoke_all_except(&administrative.session_id).await.unwrap();
    let after = sessions.list_active().await.unwrap();
    assert_eq!(before.len(), 2);
    let listed_phone = before.iter().find(|s| s.session_id.as_str() == phone.session_id).unwrap();
    assert!(listed_phone.connected);
    assert_eq!(listed_phone.client.label.as_deref(), Some("Synthetic phone"));
    let listed_admin = before.iter().find(|s| s.session_id.as_str() == administrative.session_id).unwrap();
    assert_eq!(listed_admin.client.device_type, DeviceType::Desktop);
    assert_eq!(revoked, 1);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].session_id.as_str(), administrative.session_id);
    assert_eq!(sessions.verify(&phone.token).await.unwrap_err().tag(), "SessionTokenRevokedError");
    assert_eq!(
        sessions.verify_websocket_token(&phone_ticket).await.unwrap_err().tag(),
        "WebSocketSessionRevokedError"
    );
}

#[tokio::test]
async fn persists_last_connected_at_on_first_connect_and_after_reconnect() {
    let (_db, clock, sessions) = setup();
    let issued = sessions
        .issue(IssueSessionInput {
            subject: Some("reconnect-test".into()),
            method: Some(Method::BearerAccessToken),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(sessions.list_active().await.unwrap()[0].last_connected_at, None);
    clock.advance(1000);
    sessions.mark_connected(&issued.session_id).await;
    let first = sessions.list_active().await.unwrap();
    let first_at = first[0].last_connected_at.unwrap();
    assert!(first[0].connected);
    clock.advance(1000);
    sessions.mark_connected(&issued.session_id).await;
    assert_eq!(sessions.list_active().await.unwrap()[0].last_connected_at, Some(first_at));
    sessions.mark_disconnected(&issued.session_id).await;
    sessions.mark_disconnected(&issued.session_id).await;
    let after = sessions.list_active().await.unwrap();
    assert!(!after[0].connected);
    assert_eq!(after[0].last_connected_at, Some(first_at));
    clock.advance(1000);
    sessions.mark_connected(&issued.session_id).await;
    let again = sessions.list_active().await.unwrap();
    assert!(again[0].connected);
    assert_ne!(again[0].last_connected_at, Some(first_at));
}

#[tokio::test]
async fn keeps_connected_relay_sessions_visible_through_expiry_and_renewal() {
    let (_db, clock, sessions) = setup();
    let original = sessions.issue(relay_session_input()).await.unwrap();
    let (ticket, _) = sessions.issue_websocket_token(&original.session_id, Some(2 * HOUR));
    sessions.verify_websocket_token(&ticket).await.unwrap();
    sessions.mark_connected(&original.session_id).await;
    let before = sessions.list_active().await.unwrap();
    assert_eq!(before.len(), 1);
    assert!(before[0].connected);
    clock.advance(61 * 60 * 1000);
    assert_eq!(sessions.list_active().await.unwrap(), before);
    assert_eq!(sessions.verify(&original.token).await.unwrap_err().tag(), "SessionTokenExpiredError");
    assert_eq!(
        sessions.verify_websocket_token(&ticket).await.unwrap_err().tag(),
        "WebSocketSessionExpiredError"
    );
    let renewed = sessions.issue(relay_session_input()).await.unwrap();
    let after = sessions.list_active().await.unwrap();
    assert_ne!(renewed.session_id, original.session_id);
    assert_eq!(after.len(), 2);
    assert!(after.contains(&before[0]));
    let fresh = after.iter().find(|s| s.session_id.as_str() == renewed.session_id).unwrap();
    assert!(!fresh.connected);
    assert_eq!(fresh.last_connected_at, None);
}

#[tokio::test]
async fn removes_an_expired_session_after_its_last_socket_closes() {
    for socket_count in [1usize, 2] {
        let (_db, clock, sessions) = setup();
        let issued = sessions.issue(relay_session_input()).await.unwrap();
        let mut changes = sessions.subscribe_changes();
        for _ in 0..socket_count {
            sessions.mark_connected(&issued.session_id).await;
            match changes.next().await.unwrap() {
                SessionCredentialChange::ClientUpserted(s) => {
                    assert_eq!(s.session_id.as_str(), issued.session_id);
                    assert!(s.connected);
                }
                other => panic!("{other:?}"),
            }
        }
        clock.advance(61 * 60 * 1000);
        for remaining in (0..socket_count).rev() {
            sessions.mark_disconnected(&issued.session_id).await;
            let change = changes.next().await.unwrap();
            let listed = sessions.list_active().await.unwrap();
            if remaining > 0 {
                assert!(matches!(change, SessionCredentialChange::ClientUpserted(ref s) if s.connected));
                assert_eq!(listed.len(), 1);
                assert!(listed[0].connected);
            } else {
                assert_eq!(change, SessionCredentialChange::ClientRemoved(issued.session_id.clone()));
                assert!(listed.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn removes_expired_connected_sessions_with_revoke_and_revoke_all_except() {
    for use_revoke in [true, false] {
        let (_db, clock, sessions) = setup();
        let administrative = sessions
            .issue(IssueSessionInput {
                subject: Some("desktop-bootstrap".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        let relay = sessions.issue(relay_session_input()).await.unwrap();
        sessions.mark_connected(&relay.session_id).await;
        clock.advance(61 * 60 * 1000);
        let mut changes = sessions.subscribe_changes();
        if use_revoke {
            assert!(sessions.revoke(&relay.session_id).await.unwrap());
        } else {
            assert_eq!(sessions.revoke_all_except(&administrative.session_id).await.unwrap(), 1);
        }
        assert_eq!(changes.next().await.unwrap(), SessionCredentialChange::ClientRemoved(relay.session_id.clone()));
        let listed = sessions.list_active().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id.as_str(), administrative.session_id);
    }
}

#[tokio::test]
async fn records_client_connection_metadata_without_clearing_prior_values() {
    let (db, _clock, sessions) = setup();
    let issued = sessions
        .issue(IssueSessionInput {
            subject: Some("client-connection-test".into()),
            method: Some(Method::BearerAccessToken),
            ..Default::default()
        })
        .await
        .unwrap();
    let read = || {
        let id = issued.session_id.clone();
        let db = db.clone();
        async move {
            db.call(move |conn| {
                conn.raw()
                    .query_row(
                        "SELECT client_surface, client_app_version FROM auth_sessions WHERE session_id = ?1",
                        [id],
                        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
                    )
                    .map_err(|e| DbError::sql("test", e))
            })
            .await
            .unwrap()
        }
    };
    sessions.record_client_connection(&issued.session_id, Some("mobile"), Some("1.2.0")).await;
    assert_eq!(read().await, (Some("mobile".into()), Some("1.2.0".into())));
    sessions.record_client_connection(&issued.session_id, None, Some("1.3.0")).await;
    assert_eq!(read().await, (Some("mobile".into()), Some("1.3.0".into())));
    sessions.record_client_connection(&issued.session_id, None, None).await;
    assert_eq!(read().await, (Some("mobile".into()), Some("1.3.0".into())));
}

// PairingGrantStore.test.ts

fn pairing_store() -> (Db, Arc<TestClock>, PairingGrantStore) {
    let db = Db::open_in_memory().unwrap();
    let clock = TestClock::new(START);
    let store = PairingGrantStore::new(db.clone(), clock.clone(), false);
    (db, clock, store)
}

#[tokio::test]
async fn one_time_tokens_can_only_be_consumed_once() {
    let (_db, _clock, grants) = pairing_store();
    let issued = grants
        .issue_one_time_token(IssueOneTimeTokenInput {
            label: Some("Synthetic phone".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(regex::Regex::new("^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{12}$")
        .unwrap()
        .is_match(&issued.credential));
    assert_eq!(issued.expires_at, START + 5 * 60 * 1000);
    let first = grants.consume(&issued.credential, None).await.unwrap();
    let second = grants.consume(&issued.credential, None).await.unwrap_err();
    assert_eq!(first.scopes, STANDARD_CLIENT_SCOPES.to_vec());
    assert_eq!(first.subject, "one-time-token");
    assert_eq!(first.label.as_deref(), Some("Synthetic phone"));
    assert_eq!(second, BootstrapCredentialError::UnknownBootstrapCredential);
    assert!(second.to_string().contains("Unknown bootstrap credential"));
}

#[tokio::test]
async fn atomically_consumes_a_one_time_token_when_requests_race() {
    let (_db, _clock, grants) = pairing_store();
    let grants = Arc::new(grants);
    let token = grants.issue_one_time_token(IssueOneTimeTokenInput::default()).await.unwrap();
    let results = futures::future::join_all((0..8).map(|_| {
        let grants = grants.clone();
        let credential = token.credential.clone();
        tokio::spawn(async move { grants.consume(&credential, None).await })
    }))
    .await;
    let results: Vec<_> = results.into_iter().map(|r| r.unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for failure in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(*failure, BootstrapCredentialError::UnknownBootstrapCredential);
    }
}

#[tokio::test]
async fn requires_the_bound_proof_key_thumbprint() {
    let (_db, _clock, grants) = pairing_store();
    let token = grants
        .issue_one_time_token(IssueOneTimeTokenInput {
            proof_key_thumbprint: Some("client-proof-key-thumbprint".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let missing = grants.consume(&token.credential, None).await.unwrap_err();
    let wrong = grants.consume(&token.credential, Some("other-proof-key-thumbprint")).await.unwrap_err();
    let consumed = grants.consume(&token.credential, Some("client-proof-key-thumbprint")).await.unwrap();
    assert!(missing.to_string().contains("proof key mismatch"));
    assert!(wrong.to_string().contains("proof key mismatch"));
    assert_eq!(consumed.proof_key_thumbprint.as_deref(), Some("client-proof-key-thumbprint"));
}

#[tokio::test]
async fn expired_credentials_are_reported_as_expired() {
    let (_db, clock, grants) = pairing_store();
    let token = grants.issue_one_time_token(IssueOneTimeTokenInput::default()).await.unwrap();
    clock.advance(5 * 60 * 1000);
    assert_eq!(
        grants.consume(&token.credential, None).await.unwrap_err(),
        BootstrapCredentialError::ExpiredBootstrapCredential
    );
}

#[tokio::test]
async fn keeps_credentials_out_of_pairing_lists_and_change_events() {
    let (_db, _clock, grants) = pairing_store();
    let mut changes = grants.subscribe_changes();
    for label in [None, Some("Synthetic phone")] {
        let issued = grants
            .issue_one_time_token(IssueOneTimeTokenInput {
                label: label.map(str::to_owned),
                ..Default::default()
            })
            .await
            .unwrap();
        let BootstrapCredentialChange::PairingLinkUpserted(link) = changes.next().await.unwrap() else {
            panic!("expected an upsert");
        };
        assert_eq!(link.id, issued.id);
        assert!(!serde_json::to_string(&link).unwrap().contains("credential"));
        let listed = grants.list_active().await.unwrap();
        assert_eq!(listed.iter().find(|l| l.id == issued.id), Some(&link));
        let consumed = grants.consume(&issued.credential, None).await.unwrap();
        assert_eq!(consumed.scopes, link.scopes);
        assert_eq!(changes.next().await.unwrap(), BootstrapCredentialChange::PairingLinkRemoved(issued.id.clone()));
    }
}

#[tokio::test]
async fn lists_and_revokes_active_pairing_links() {
    let (_db, _clock, grants) = pairing_store();
    let first = grants.issue_one_time_token(IssueOneTimeTokenInput::default()).await.unwrap();
    let second = grants
        .issue_one_time_token(IssueOneTimeTokenInput {
            scopes: Some(vec![Scope::OrchestrationRead, Scope::AccessWrite]),
            ..Default::default()
        })
        .await
        .unwrap();
    let before: Vec<String> = grants.list_active().await.unwrap().into_iter().map(|l| l.id).collect();
    assert!(before.contains(&first.id) && before.contains(&second.id));
    assert!(grants.revoke(&first.id).await.unwrap());
    let after: Vec<String> = grants.list_active().await.unwrap().into_iter().map(|l| l.id).collect();
    assert!(!after.contains(&first.id) && after.contains(&second.id));
    let error = grants.consume(&first.credential, None).await.unwrap_err();
    assert!(error.to_string().contains("no longer available"));
    assert_eq!(error.tag(), "UnavailableBootstrapCredentialError");
    assert!(!grants.revoke(&first.id).await.unwrap());
}

#[tokio::test]
async fn identifies_consume_available_failures() {
    let (db, _clock, grants) = pairing_store();
    exec(&db, "DROP TABLE auth_pairing_links").await;
    let error = grants.consume("credential", None).await.unwrap_err();
    assert_eq!(error.tag(), "BootstrapCredentialConsumeAvailableError");
    assert!(error.is_internal());
}

// EnvironmentAuth.test.ts, EnvironmentAuthAdmin.test.ts

async fn environment_auth(host: &str) -> (tempfile::TempDir, Arc<TestClock>, Arc<EnvironmentAuth>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let secrets = ServerSecretStore::open(dir.path().join("secrets")).await.unwrap();
    let clock = TestClock::new(START);
    let auth = EnvironmentAuth::open(db, secrets, cookie_input(host), clock.clone()).await.unwrap();
    (dir, clock, Arc::new(auth))
}

fn request_metadata() -> AuthClientMetadata {
    AuthClientMetadata {
        label: None,
        ip_address: Some("127.0.0.1".into()),
        user_agent: Some("test-agent".into()),
        device_type: DeviceType::Desktop,
        os: Some("macOS".into()),
        browser: Some("Chrome".into()),
    }
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        map.append(http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
    }
    map
}

async fn authenticate(auth: &EnvironmentAuth, map: &HeaderMap) -> Result<zc_auth::AuthenticatedSession, ServerAuthError> {
    auth.authenticate_request(&AuthRequest {
        method: "GET",
        headers: map,
        path_and_query: "/api/auth/session",
        peer: None,
    })
    .await
}

#[tokio::test]
async fn classifies_bootstrap_failures_for_the_http_boundary() {
    let invalid = to_bootstrap_exchange_error(BootstrapCredentialError::UnknownBootstrapCredential);
    assert_eq!(invalid.tag(), "ServerAuthInvalidCredentialError");
    let internal = to_bootstrap_exchange_error(BootstrapCredentialError::BootstrapCredentialConsume {
        cause: "sqlite is unavailable".into(),
    });
    assert_eq!(internal.tag(), "ServerAuthBootstrapCredentialValidationError");
    assert_eq!(internal.to_string(), "Failed to validate bootstrap credential.");
}

#[tokio::test]
async fn issues_standard_pairing_credentials_by_default() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let pairing = auth.issue_pairing_credential(None, None).await.unwrap();
    let exchanged = auth.create_browser_session(&pairing.credential, request_metadata()).await.unwrap();
    let cookie = format!("{}={}", auth.sessions().cookie_name(), exchanged.session_token);
    let verified = authenticate(&auth, &headers(&[("cookie", &cookie)])).await.unwrap();
    assert!(!verified.session_id.is_empty());
    assert_eq!(verified.scopes, STANDARD_CLIENT_SCOPES.to_vec());
    assert_eq!(verified.subject, "one-time-token");
    assert_eq!(verified.method, Method::BrowserSessionCookie);
}

#[tokio::test]
async fn prefers_a_bearer_token_over_a_stale_legacy_cookie() {
    let (_dir, _clock, auth) = environment_auth("192.168.1.50").await;
    let bearer = auth.issue_session(IssueBearerSessionInput::default()).await.unwrap();
    assert_eq!(auth.sessions().legacy_cookie_name(), Some("t3_session"));
    let verified = authenticate(
        &auth,
        &headers(&[("cookie", "t3_session=stale"), ("authorization", &format!("Bearer {}", bearer.token))]),
    )
    .await
    .unwrap();
    assert_eq!(verified.session_id, bearer.session_id);
}

#[tokio::test]
async fn missing_and_invalid_credentials() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    assert_eq!(authenticate(&auth, &HeaderMap::new()).await.unwrap_err(), ServerAuthError::MissingCredential);
    // An empty session cookie is selected, then found missing (no fallback to bearer).
    let cookie = format!("{}=", auth.sessions().cookie_name());
    let bearer = auth.issue_session(IssueBearerSessionInput::default()).await.unwrap();
    assert_eq!(
        authenticate(&auth, &headers(&[("cookie", &cookie), ("authorization", &format!("Bearer {}", bearer.token))]))
            .await
            .unwrap_err(),
        ServerAuthError::MissingCredential
    );
    let error = authenticate(&auth, &headers(&[("authorization", "Bearer nope")])).await.unwrap_err();
    assert_eq!(error.credential_reason(), "invalid_credential");
    // A DPoP authorization needs a proof-bound token.
    let error = authenticate(&auth, &headers(&[("authorization", &format!("DPoP {}", bearer.token))]))
        .await
        .unwrap_err();
    assert_eq!(error.dpop_failure_reason(), Some(zc_contracts::DpopFailureReason::InvalidProof));
}

#[tokio::test]
async fn does_not_exchange_ordinary_grants_for_administrative_tokens() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let pairing = auth.issue_pairing_credential(None, None).await.unwrap();
    let error = auth
        .exchange_bootstrap_credential_for_access_token(
            &pairing.credential,
            Some(vec![Scope::OrchestrationRead, Scope::AccessWrite]),
            request_metadata(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error, ServerAuthError::ScopeNotGranted);
}

#[tokio::test]
async fn inherits_a_constrained_grant_when_token_exchange_omits_scope() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let pairing = auth.issue_pairing_credential(None, Some(vec![Scope::OrchestrationRead])).await.unwrap();
    let token = auth
        .exchange_bootstrap_credential_for_access_token(&pairing.credential, None, request_metadata(), None)
        .await
        .unwrap();
    assert_eq!(token.scope, "orchestration:read");
    assert_eq!(token.expires_in.get(), (30 * 24 * HOUR / 1000) as f64);
    assert_eq!(
        serde_json::to_value(&token).unwrap()["issued_token_type"],
        "urn:ietf:params:oauth:token-type:access_token"
    );
}

#[tokio::test]
async fn user_issued_administrative_pairing_links_stay_listed() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let pairing = auth.issue_pairing_credential(None, Some(ADMINISTRATIVE_SCOPES.to_vec())).await.unwrap();
    let listed = auth.list_pairing_links(None).await.unwrap();
    assert_eq!(listed.iter().find(|l| l.id == pairing.id).unwrap().subject, "one-time-token");
}

#[tokio::test]
async fn startup_pairing_urls_bootstrap_hidden_administrative_sessions() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let url = auth.issue_startup_pairing_url("http://127.0.0.1:3773").await.unwrap();
    let token = url.split("#token=").nth(1).unwrap().to_owned();
    assert!(url.starts_with("http://127.0.0.1:3773/pair#token="));
    let listed = auth.list_pairing_links(None).await.unwrap();
    assert!(!listed.iter().any(|l| l.subject == "administrative-bootstrap"));
    let all = auth.list_pairing_links(Some(&[])).await.unwrap();
    assert!(all.iter().any(|l| l.subject == "administrative-bootstrap"));
    let exchanged = auth.create_browser_session(&token, request_metadata()).await.unwrap();
    let cookie = format!("{}={}", auth.sessions().cookie_name(), exchanged.session_token);
    let verified = authenticate(&auth, &headers(&[("cookie", &cookie)])).await.unwrap();
    assert_eq!(verified.scopes, ADMINISTRATIVE_SCOPES.to_vec());
    assert_eq!(verified.subject, "administrative-bootstrap");
}

#[tokio::test]
async fn lists_pairing_links_and_revokes_other_sessions_keeping_the_administrator() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let startup = auth.issue_startup_pairing_credential().await.unwrap();
    let admin = auth.create_browser_session(&startup.credential, request_metadata()).await.unwrap();
    let admin_cookie = format!("{}={}", auth.sessions().cookie_name(), admin.session_token);
    let admin_session = authenticate(&auth, &headers(&[("cookie", &admin_cookie)])).await.unwrap();
    let pairing = auth.issue_pairing_credential(Some("Synthetic phone".into()), None).await.unwrap();
    let listed = auth.list_pairing_links(None).await.unwrap();
    let phone = auth
        .create_browser_session(
            &pairing.credential,
            AuthClientMetadata {
                device_type: DeviceType::Mobile,
                os: Some("iOS".into()),
                browser: Some("Safari".into()),
                ip_address: Some("192.168.1.88".into()),
                ..request_metadata()
            },
        )
        .await
        .unwrap();
    let phone_cookie = format!("{}={}", auth.sessions().cookie_name(), phone.session_token);
    let phone_session = authenticate(&auth, &headers(&[("cookie", &phone_cookie)])).await.unwrap();
    let before = auth.list_client_sessions(&admin_session.session_id).await.unwrap();
    let revoked = auth.revoke_other_client_sessions(&admin_session.session_id).await.unwrap();
    let after = auth.list_client_sessions(&admin_session.session_id).await.unwrap();
    assert!(listed.iter().any(|l| l.id == pairing.id && l.label.as_deref() == Some("Synthetic phone")));
    assert_eq!(before.len(), 2);
    assert!(before.iter().find(|s| s.session_id.as_str() == admin_session.session_id).unwrap().current);
    let listed_phone = before.iter().find(|s| s.session_id.as_str() == phone_session.session_id).unwrap();
    assert!(!listed_phone.current);
    assert_eq!(listed_phone.client.label.as_deref(), Some("Synthetic phone"));
    assert_eq!(listed_phone.client.device_type, DeviceType::Mobile);
    // Managers are listed first.
    assert_eq!(before[0].session_id.as_str(), admin_session.session_id);
    assert_eq!(revoked, 1);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].session_id.as_str(), admin_session.session_id);
    assert_eq!(
        auth.revoke_client_session(&admin_session.session_id, &admin_session.session_id)
            .await
            .unwrap_err(),
        ServerAuthError::ForbiddenOperation
    );
}

#[tokio::test]
async fn creates_lists_and_revokes_client_pairing_links() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let created = auth
        .create_pairing_link(CreatePairingLinkInput {
            scopes: Some(vec![Scope::OrchestrationRead]),
            subject: Some("one-time-token".into()),
            label: Some("CI phone".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let before = auth.list_pairing_links(None).await.unwrap();
    assert!(auth.revoke_pairing_link(&created.id).await.unwrap());
    let after = auth.list_pairing_links(None).await.unwrap();
    assert_eq!(created.scopes, vec![Scope::OrchestrationRead]);
    assert!(!created.credential.is_empty());
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].id, created.id);
    assert_eq!(before[0].label.as_deref(), Some("CI phone"));
    assert!(!serde_json::to_string(&before[0]).unwrap().contains("credential"));
    assert!(after.is_empty());
}

#[tokio::test]
async fn issues_bearer_sessions_and_surfaces_last_connected_at() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let issued = auth
        .issue_session(IssueBearerSessionInput {
            label: Some("deploy-bot".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    let verified = auth.sessions().verify(&issued.token).await.unwrap();
    assert_eq!(issued.scopes, ADMINISTRATIVE_SCOPES.to_vec());
    assert_eq!(issued.client.device_type, DeviceType::Bot);
    assert_eq!(issued.client.label.as_deref(), Some("deploy-bot"));
    assert_eq!(issued.subject, "cli-issued-session");
    assert_eq!(verified.session_id, issued.session_id);
    assert_eq!(verified.method, Method::BearerAccessToken);
    let before = auth.list_sessions().await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].last_connected_at, None);
    auth.sessions().mark_connected(&issued.session_id).await;
    assert!(auth.list_sessions().await.unwrap()[0].last_connected_at.is_some());
    assert!(auth.revoke_session(&issued.session_id).await.unwrap());
    assert!(auth.list_sessions().await.unwrap().is_empty());
}

#[tokio::test]
async fn websocket_upgrades_prefer_the_ticket() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let bearer = auth.issue_session(IssueBearerSessionInput::default()).await.unwrap();
    let ticket = auth.issue_websocket_ticket(&bearer.session_id);
    let request_headers = HeaderMap::new();
    let request = AuthRequest {
        method: "GET",
        headers: &request_headers,
        path_and_query: "/ws",
        peer: None,
    };
    let session = auth.authenticate_websocket_upgrade(&request, Some(&ticket.ticket)).await.unwrap();
    assert_eq!(session.session_id, bearer.session_id);
    assert_eq!(
        auth.authenticate_websocket_upgrade(&request, Some("  ")).await.unwrap_err(),
        ServerAuthError::MissingCredential
    );
    assert_eq!(
        auth.authenticate_websocket_upgrade(&request, Some("bad"))
            .await
            .unwrap_err()
            .credential_reason(),
        "invalid_credential"
    );
}

// ws.ts subscribeAuthAccess
#[tokio::test]
async fn auth_access_stream_sends_a_snapshot_then_numbered_changes() {
    let (_dir, _clock, auth) = environment_auth("127.0.0.1").await;
    let owner = auth.issue_session(IssueBearerSessionInput::default()).await.unwrap();
    let existing = auth.issue_pairing_credential(None, None).await.unwrap();
    let stream = zc_auth::ws::subscribe_auth_access(&auth, owner.session_id.clone()).await.unwrap();
    futures::pin_mut!(stream);
    let snapshot = stream.next().await.unwrap().unwrap();
    assert_eq!(snapshot["type"], "snapshot");
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(snapshot["version"], 1);
    assert_eq!(snapshot["payload"]["pairingLinks"][0]["id"], existing.id.as_str());
    assert_eq!(snapshot["payload"]["clientSessions"][0]["current"], true);
    assert!(!snapshot.to_string().contains("credential"));

    let created = auth.issue_pairing_credential(Some("Synthetic tablet".into()), None).await.unwrap();
    let upsert = stream.next().await.unwrap().unwrap();
    assert_eq!(upsert["type"], "pairingLinkUpserted");
    assert_eq!(upsert["revision"], 2);
    assert_eq!(upsert["payload"]["id"], created.id.as_str());
    assert!(!upsert.to_string().contains(&created.credential));

    auth.sessions().mark_connected(&owner.session_id).await;
    let connected = stream.next().await.unwrap().unwrap();
    assert_eq!(connected["type"], "clientUpserted");
    assert_eq!(connected["revision"], 3);
    assert_eq!(connected["payload"]["connected"], true);
    assert_eq!(connected["payload"]["current"], true);

    let other = auth.issue_session(IssueBearerSessionInput::default()).await.unwrap();
    let issued = stream.next().await.unwrap().unwrap();
    assert_eq!(issued["type"], "clientUpserted");
    assert_eq!(issued["payload"]["current"], false);
    auth.revoke_session(&other.session_id).await.unwrap();
    let removed = stream.next().await.unwrap().unwrap();
    assert_eq!(removed["type"], "clientRemoved");
    assert_eq!(removed["revision"], 5);
    assert_eq!(removed["payload"]["sessionId"], other.session_id.as_str());

    assert!(auth.revoke_pairing_link(&created.id).await.unwrap());
    let link_removed = stream.next().await.unwrap().unwrap();
    assert_eq!(link_removed["type"], "pairingLinkRemoved");
    assert_eq!(link_removed["payload"]["id"], created.id.as_str());
}
