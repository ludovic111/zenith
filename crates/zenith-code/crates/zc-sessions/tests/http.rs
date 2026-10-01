//! `GET /api/zenith/sessions` end to end, on made-up logs.

use std::fs;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use zc_http::{DevAuthenticator, EnvironmentError};
use zc_rpc::AuthContext;
use zc_sessions::{router, HttpAuthRequest, HttpAuthenticator, ProjectRoot, ReaderConfig, SessionReader, SessionsApi, StaticProjectRoots, WsAuthBridge};

/// Accepts `Authorization: Bearer good` only, with the given scopes.
struct BearerAuth(Vec<&'static str>);

#[async_trait]
impl HttpAuthenticator for BearerAuth {
    async fn authenticate(&self, request: &HttpAuthRequest<'_>) -> Result<AuthContext, EnvironmentError> {
        match request.headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer good") => Ok(AuthContext::new(self.0.iter().copied())),
            Some(_) => Err(EnvironmentError::invalid_credential()),
            None => Err(EnvironmentError::missing_credential()),
        }
    }
}

fn now_iso(minus_secs: i64) -> String {
    (jiff::Timestamp::now() - jiff::SignedDuration::from_secs(minus_secs)).to_string()
}

fn logs() -> (tempfile::TempDir, ReaderConfig) {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("claude/projects/-work-acme");
    fs::create_dir_all(&claude).unwrap();
    let live = format!(
        "{}\n{}\n{}\n",
        format_args!(
            r#"{{"type":"user","cwd":"/work/acme/api","gitBranch":"claude/fix-login","turnOrigin":"human","timestamp":"{}"}}"#,
            now_iso(600)
        ),
        format_args!(
            r#"{{"type":"assistant","cwd":"/work/acme/api","timestamp":"{}","message":{{"model":"claude-opus-5-5","usage":{{"output_tokens":42}}}}}}"#,
            now_iso(30)
        ),
        r#"{"type":"cost-state","totalCostUSD":0.75,"totalLinesAdded":12,"totalLinesRemoved":3}"#,
    );
    fs::write(claude.join("live-1.jsonl"), live).unwrap();
    let old = r#"{"type":"user","cwd":"/elsewhere","turnOrigin":"human","timestamp":"2026-01-02T10:00:00Z"}
{"type":"last-prompt","lastPrompt":"sketch a logo"}
"#;
    fs::write(claude.join("old-1.jsonl"), old).unwrap();
    let codex = dir.path().join("codex/sessions/2026/09/30");
    fs::create_dir_all(&codex).unwrap();
    let rollout = format!(
        r#"{{"timestamp":"{t}","type":"session_meta","payload":{{"id":"cx-1","timestamp":"{t}","cwd":"/work/acme","originator":"codex_cli_rs"}}}}
{{"timestamp":"{t}","type":"event_msg","payload":{{"type":"user_message","message":"write the release notes"}}}}
"#,
        t = now_iso(3 * 86_400)
    );
    fs::write(codex.join("rollout-1.jsonl"), rollout).unwrap();
    let config = ReaderConfig {
        claude_home: dir.path().join("claude"),
        codex_home: dir.path().join("codex"),
        ttl: Duration::from_secs(20),
    };
    (dir, config)
}

fn app(config: ReaderConfig, auth: Arc<dyn HttpAuthenticator>) -> axum::Router {
    router(SessionsApi {
        reader: Arc::new(SessionReader::new(config)),
        projects: Arc::new(StaticProjectRoots(vec![ProjectRoot {
            id: "acme".into(),
            title: "Acme".into(),
            workspace_root: "/work/acme".into(),
        }])),
        auth,
    })
}

async fn get(app: &axum::Router, uri: &str, bearer: Option<&str>) -> (StatusCode, http::HeaderMap, Value) {
    let mut req = Request::get(uri);
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    let res = app.clone().oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let (parts, body) = res.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, parts.headers, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn answers_sessions_and_totals() {
    let (_dir, config) = logs();
    let app = app(config, Arc::new(BearerAuth(vec!["orchestration:read"])));
    let (status, headers, body) = get(&app, "/api/zenith/sessions?tz=UTC", Some("good")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(body["timeZone"], "UTC");

    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 3);
    let first = &sessions[0];
    assert_eq!(first["id"], "live-1");
    assert_eq!(first["live"], true);
    assert_eq!(first["project"], serde_json::json!({"id": "acme", "title": "Acme"}));
    assert_eq!(first["branch"], "claude/fix-login");
    assert_eq!(first["costUSD"], 0.75);
    assert_eq!(first["resume"], "cd /work/acme/api && claude --resume live-1");
    assert_eq!(sessions[1]["agent"], "codex");
    assert_eq!(sessions[1]["title"], "write the release notes");
    assert_eq!(sessions[2]["title"], "sketch a logo");
    assert_eq!(sessions[2]["project"], Value::Null);

    let totals = &body["totals"];
    assert_eq!((totals["sessions"].as_u64(), totals["live"].as_u64()), (Some(3), Some(1)));
    assert_eq!(totals["week"]["sessions"], 2);
    assert_eq!(totals["week"]["linesAdded"], 12);
    assert_eq!(totals["perDay"].as_array().unwrap().len(), 21);
    assert_eq!(totals["perProject"][0]["project"]["id"], "acme");
    assert_eq!(totals["perProject"][0]["sessions"], 2);

    let (_, _, limited) = get(&app, "/api/zenith/sessions?limit=1", Some("good")).await;
    assert_eq!(limited["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(limited["totals"]["sessions"], 3);
}

#[tokio::test]
async fn refuses_without_credential_or_scope() {
    let (_dir, config) = logs();
    let app_ok = app(config.clone(), Arc::new(BearerAuth(vec!["orchestration:read"])));
    let (status, _, body) = get(&app_ok, "/api/zenith/sessions", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["_tag"], "EnvironmentAuthInvalidError");
    assert_eq!(body["reason"], "missing_credential");
    let (status, _, _) = get(&app_ok, "/api/zenith/sessions", Some("bad")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let app_no_scope = app(config, Arc::new(BearerAuth(vec!["terminal:operate"])));
    let (status, _, body) = get(&app_no_scope, "/api/zenith/sessions", Some("good")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["requiredScope"], "orchestration:read");
}

#[tokio::test]
async fn the_ws_authenticator_can_stand_in() {
    let (_dir, config) = logs();
    let dev = DevAuthenticator {
        scopes: vec!["orchestration:read".into()],
    };
    let app = app(config, Arc::new(WsAuthBridge(Arc::new(dev))));
    let (status, _, _) = get(&app, "/api/zenith/sessions", None).await;
    assert_eq!(status, StatusCode::OK);
}
