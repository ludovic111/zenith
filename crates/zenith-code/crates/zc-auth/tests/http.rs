//! The auth HTTP routes through axum, for the edges the black-box suite does not reach
//! (HttpApi payload handling, the legacy cookie migration, header details).

use std::sync::Arc;

use axum::body::Body;
use axum::Router;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use zc_auth::{CookieNameInput, EnvironmentAuth, IssueBearerSessionInput, ServerMode, TestClock};
use zc_core::ServerSecretStore;
use zc_db::Db;

struct App {
    _dir: tempfile::TempDir,
    auth: Arc<EnvironmentAuth>,
    router: Router,
}

async fn app(host: &str) -> App {
    let dir = tempfile::tempdir().unwrap();
    let secrets = ServerSecretStore::open(dir.path().join("secrets")).await.unwrap();
    let cookie = CookieNameInput {
        mode: ServerMode::Web,
        port: 3773,
        host: Some(host.into()),
        instance_key: "/tmp/t3-auth-http-test".into(),
        environment_id: "test-environment".into(),
        development: false,
    };
    let auth = Arc::new(
        EnvironmentAuth::open(Db::open_in_memory().unwrap(), secrets, cookie, TestClock::new(1_790_000_000_000))
            .await
            .unwrap(),
    );
    App {
        _dir: dir,
        router: zc_auth::http::routes(auth.clone()),
        auth,
    }
}

struct Answer {
    status: StatusCode,
    headers: http::HeaderMap,
    text: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap()
    }
}

async fn send(app: &App, request: Request<Body>) -> Answer {
    let response = app.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Answer {
        status,
        headers,
        text: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

fn post(path: &str, content_type: Option<&str>, body: &str) -> Request<Body> {
    let mut builder = Request::post(path);
    if let Some(content_type) = content_type {
        builder = builder.header("content-type", content_type);
    }
    builder.body(Body::from(body.to_owned())).unwrap()
}

#[tokio::test]
async fn session_state_without_credentials() {
    let app = app("127.0.0.1").await;
    let answer = send(&app, Request::get("/api/auth/session").body(Body::empty()).unwrap()).await;
    assert_eq!(answer.status, StatusCode::OK);
    let body = answer.json();
    assert_eq!(body["authenticated"], false);
    assert_eq!(body["auth"]["policy"], "loopback-browser");
    assert_eq!(body.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["authenticated", "auth"]);
    assert!(answer.headers.get("cache-control").is_none());
}

#[tokio::test]
async fn browser_session_payload_handling() {
    let app = app("127.0.0.1").await;
    let wrong_type = send(&app, post("/api/auth/browser-session", Some("text/plain"), "{}")).await;
    assert_eq!(wrong_type.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(wrong_type.text, "Unsupported content-type: text/plain");
    let empty = send(&app, post("/api/auth/browser-session", Some("application/json"), "")).await;
    assert_eq!((empty.status, empty.text.as_str()), (StatusCode::BAD_REQUEST, ""));
    let blank = send(&app, post("/api/auth/browser-session", None, r#"{"credential":"  "}"#)).await;
    assert_eq!(blank.status, StatusCode::BAD_REQUEST);
    let unknown = send(
        &app,
        post(
            "/api/auth/browser-session",
            Some("application/json; charset=utf-8"),
            r#"{"credential":"ABCDEFGHJKLM"}"#,
        ),
    )
    .await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert_eq!(unknown.json()["reason"], "invalid_credential");
    assert!(unknown.headers.get("set-cookie").is_none());
}

#[tokio::test]
async fn browser_session_sets_the_cookie_and_no_store() {
    let app = app("127.0.0.1").await;
    let credential = app.auth.issue_pairing_credential(None, None).await.unwrap().credential;
    let answer = send(&app, post("/api/auth/browser-session", None, &format!(r#"{{"credential":"  {credential} "}}"#))).await;
    assert_eq!(answer.status, StatusCode::OK, "{}", answer.text);
    assert_eq!(answer.headers["cache-control"], "no-store");
    assert_eq!(answer.headers["pragma"], "no-cache");
    let cookie = answer.headers["set-cookie"].to_str().unwrap();
    let name = app.auth.sessions().cookie_name();
    assert!(cookie.starts_with(&format!("{name}=")), "{cookie}");
    assert!(
        cookie.ends_with("; Path=/; Expires=Wed, 21 Oct 2026 14:13:20 GMT; HttpOnly; SameSite=Lax"),
        "{cookie}"
    );
    let body = answer.json();
    assert_eq!(
        body.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["authenticated", "scopes", "sessionMethod", "expiresAt"]
    );
    assert_eq!(body["expiresAt"], "2026-10-21T14:13:20.000Z");
}

#[tokio::test]
async fn migrates_the_legacy_cookie_of_remote_servers() {
    let app = app("192.168.1.50").await;
    let credential = app.auth.issue_pairing_credential(None, None).await.unwrap().credential;
    let exchange = app
        .auth
        .create_browser_session(&credential, zc_auth::client_metadata::derive_auth_client_metadata(None, None, None))
        .await
        .unwrap();
    let request = Request::get("/api/auth/session")
        .header("cookie", format!("t3_session={}", exchange.session_token))
        .body(Body::empty())
        .unwrap();
    let answer = send(&app, request).await;
    assert_eq!(answer.json()["authenticated"], true);
    let cookie = answer.headers["set-cookie"].to_str().unwrap();
    assert!(cookie.starts_with(&format!("{}={}", app.auth.sessions().cookie_name(), exchange.session_token)));
    assert_eq!(answer.headers["cache-control"], "no-store");
}

#[tokio::test]
async fn authenticated_routes_check_the_session_before_the_body() {
    let app = app("127.0.0.1").await;
    let answer = send(&app, post("/api/auth/pairing-token", Some("text/plain"), "garbage")).await;
    assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    assert_eq!(answer.json()["reason"], "missing_credential");
    let bearer = app.auth.issue_session(IssueBearerSessionInput::default()).await.unwrap();
    let request = Request::post("/api/auth/pairing-token")
        .header("authorization", format!("Bearer {}", bearer.token))
        .header("content-type", "application/json")
        .body(Body::from(r#"{"scopes":["nope"]}"#))
        .unwrap();
    assert_eq!(send(&app, request).await.status, StatusCode::BAD_REQUEST);
    let own = Request::post("/api/auth/clients/revoke")
        .header("authorization", format!("Bearer {}", bearer.token))
        .body(Body::from(format!(r#"{{"sessionId":"{}"}}"#, bearer.session_id)))
        .unwrap();
    let answer = send(&app, own).await;
    assert_eq!(answer.status, StatusCode::FORBIDDEN);
    assert_eq!(answer.json()["_tag"], "EnvironmentOperationForbiddenError");
    assert_eq!(answer.json()["reason"], "current_session_revoke_not_allowed");
}

#[tokio::test]
async fn token_endpoint_payload_handling() {
    let app = app("127.0.0.1").await;
    let json = send(&app, post("/oauth/token", None, "{}")).await;
    assert_eq!(json.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(json.text, "Unsupported content-type: application/json");
    let credential = app.auth.issue_pairing_credential(None, None).await.unwrap().credential;
    let form = |extra: &str| {
        format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&subject_token={credential}&subject_token_type=urn%3At3%3Aparams%3Aoauth%3Atoken-type%3Aenvironment-bootstrap&requested_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aaccess_token{extra}"
        )
    };
    let urlencoded = Some("application/x-www-form-urlencoded");
    let bad_scope = send(&app, post("/oauth/token", urlencoded, &form("&scope=root"))).await;
    assert_eq!(bad_scope.status, StatusCode::BAD_REQUEST);
    assert_eq!(bad_scope.json()["reason"], "invalid_scope");
    let not_granted = send(&app, post("/oauth/token", urlencoded, &form("&scope=access%3Awrite"))).await;
    assert_eq!(not_granted.json()["reason"], "scope_not_granted");
    // The credential was consumed by the refused exchange, like in TS.
    let consumed = send(&app, post("/oauth/token", urlencoded, &form(""))).await;
    assert_eq!(consumed.status, StatusCode::UNAUTHORIZED);
    let duplicate = send(&app, post("/oauth/token", urlencoded, &form("&scope=a&scope=b"))).await;
    assert_eq!((duplicate.status, duplicate.text.as_str()), (StatusCode::BAD_REQUEST, ""));
}
