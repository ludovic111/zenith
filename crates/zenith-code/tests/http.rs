//! The HTTP side of the skeleton, through the real router (no socket).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::Router;
use http::{header, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use zc_http::{CorsPolicy, EnvironmentError, ReadinessGate, UpgradeRequest, WsAuthenticator};
use zc_rpc::{AuthContext, RpcRouter, RpcServer};
use zenith_code::server::{router, AppServices, HttpConfig};

struct RejectAll;

#[async_trait]
impl WsAuthenticator for RejectAll {
    async fn authenticate(&self, _request: &UpgradeRequest<'_>) -> Result<AuthContext, EnvironmentError> {
        Err(EnvironmentError::missing_credential())
    }
}

fn site(dir: &Path) {
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::create_dir_all(dir.join(".vite")).unwrap();
    std::fs::create_dir_all(dir.join("docs")).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><title>root</title>").unwrap();
    std::fs::write(dir.join("docs/index.html"), "<!doctype html><title>docs</title>").unwrap();
    std::fs::write(dir.join("assets/index-AbCd1234.js"), "x".repeat(4096)).unwrap();
    std::fs::write(dir.join("assets/loose-AbCd1234.js"), "console.log(1)").unwrap();
    std::fs::write(
        dir.join(".vite/manifest.json"),
        r#"{"index.html":{"file":"assets/index-AbCd1234.js","css":[]}}"#,
    )
    .unwrap();
}

fn app(static_dir: Option<&Path>, readiness: ReadinessGate) -> Router {
    let config = HttpConfig {
        static_dir: static_dir.map(Path::to_path_buf),
        parent_origins: vec!["http://127.0.0.1:4747".into(), "http://127.0.0.1:4748".into()],
        cors: CorsPolicy::browser_api(),
        ws_origin_check: None,
    };
    let services = AppServices {
        rpc: RpcServer::new(RpcRouter::builder().build().unwrap()),
        auth: Arc::new(RejectAll),
        readiness,
    };
    router(&config, &services)
}

async fn get(app: &Router, path: &str, headers: &[(&str, &str)]) -> (StatusCode, http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().uri(path);
    for (k, v) in headers {
        request = request.header(*k, *v);
    }
    let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, body)
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("zc-http-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[tokio::test]
async fn spa_routes_and_csp_only_on_html() {
    let dir = temp_dir("spa");
    site(&dir);
    let app = app(Some(&dir), ReadinessGate::ready());
    let csp = "frame-ancestors 'self' http://127.0.0.1:4747 http://127.0.0.1:4748";

    for path in ["/", "/threads/abc", "/missing.png"] {
        let (status, headers, body) = get(&app, path, &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(headers[header::CONTENT_TYPE], "text/html; charset=utf-8", "{path}");
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], csp, "{path}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert!(headers.get(header::ETAG).is_none(), "HTML has no ETag");
        assert!(String::from_utf8(body).unwrap().contains("root"), "{path}");
    }
    let (_, _, body) = get(&app, "/docs", &[]).await;
    assert!(String::from_utf8(body).unwrap().contains("docs"), "extensionless path → dir/index.html");

    let (status, headers, _) = get(&app, "/assets/index-AbCd1234.js", &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CONTENT_SECURITY_POLICY).is_none(), "no CSP on JS");
    assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
    assert!(headers[header::CONTENT_TYPE].to_str().unwrap().contains("javascript"));
    let etag = headers[header::ETAG].to_str().unwrap().to_owned();
    assert!(etag.starts_with("W/\""));

    let (_, headers, _) = get(&app, "/assets/loose-AbCd1234.js", &[]).await;
    assert_eq!(headers[header::CACHE_CONTROL], "no-cache", "not in the manifest");

    let (status, headers, body) = get(&app, "/assets/index-AbCd1234.js", &[("if-none-match", &etag)]).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
    assert_eq!(headers[header::ETAG].to_str().unwrap(), etag);
    let strong = etag.trim_start_matches("W/");
    let (status, _, _) = get(&app, "/assets/index-AbCd1234.js", &[("if-none-match", strong)]).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED, "weak comparison");
    let (status, _, _) = get(&app, "/", &[("if-none-match", "*")]).await;
    assert_eq!(status, StatusCode::OK, "HTML never answers 304");

    let (status, _, body) = get(&app, "/../etc/passwd", &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", String::from_utf8_lossy(&body));
    let (status, _, _) = get(&app, "/a/../../etc/passwd", &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, headers, body) = get(&app, "/assets/index-AbCd1234.js", &[("accept-encoding", "gzip")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_ENCODING], "gzip");
    assert!(body.len() < 4096);

    let response = app
        .clone()
        .oneshot(Request::builder().method(Method::HEAD).uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.unwrap().to_bytes().is_empty());

    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn no_static_dir_is_503_and_missing_index_is_404() {
    let app_without = app(None, ReadinessGate::ready());
    assert_eq!(get(&app_without, "/", &[]).await.0, StatusCode::SERVICE_UNAVAILABLE);

    let dir = temp_dir("empty");
    std::fs::create_dir_all(&dir).unwrap();
    let app_empty = app(Some(&dir), ReadinessGate::ready());
    assert_eq!(get(&app_empty, "/nope", &[]).await.0, StatusCode::NOT_FOUND);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn embed_json_and_cors() {
    let app = app(None, ReadinessGate::ready());
    let (status, headers, body) = get(&app, "/zenith/embed.json", &[("origin", "http://elsewhere.test")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert!(headers.get(header::CONTENT_SECURITY_POLICY).is_none());
    assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert!(headers.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none());
    assert_eq!(
        String::from_utf8(body).unwrap(),
        r#"{"parentOrigins":["http://127.0.0.1:4747","http://127.0.0.1:4748"]}"#
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/auth/session")
                .header("origin", "http://elsewhere.test")
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let h = response.headers();
    assert_eq!(h[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
    assert_eq!(h[header::ACCESS_CONTROL_ALLOW_METHODS], "GET, POST, OPTIONS");
    assert_eq!(h[header::ACCESS_CONTROL_ALLOW_HEADERS], "authorization,b3,traceparent,content-type,dpop");
    assert_eq!(h[header::ACCESS_CONTROL_MAX_AGE], "600");
}

#[tokio::test]
async fn ws_auth_failure_is_a_typed_401() {
    let app = app(None, ReadinessGate::ready());
    let (status, headers, body) = get(&app, "/ws?wsTicket=nope", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["_tag"], "EnvironmentAuthInvalidError");
    assert_eq!(body["code"], "auth_invalid");
    assert_eq!(body["reason"], "missing_credential");
    assert_eq!(body["traceId"].as_str().unwrap().len(), 32);
}

#[tokio::test]
async fn requests_wait_for_readiness() {
    let gate = ReadinessGate::new();
    let app = app(None, gate.clone());
    let pending = tokio::spawn({
        let app = app.clone();
        async move { get(&app, "/zenith/embed.json", &[]).await.0 }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!pending.is_finished(), "held until startup finishes");
    gate.mark_ready();
    assert_eq!(pending.await.unwrap(), StatusCode::OK);

    let failed = ReadinessGate::new();
    failed.mark_failed("migrations failed");
    let app = self::app(None, failed);
    assert_eq!(get(&app, "/zenith/embed.json", &[]).await.0, StatusCode::INTERNAL_SERVER_ERROR);
}
