//! The axum app: route table, middlewares, readiness gate and graceful shutdown
//! (`server.ts` `makeRoutesLayer`, plan §1.1–1.2 and §6.18).
//!
//! Routes so far: `GET /ws` (Effect RPC), `GET /zenith/embed.json`, what the service
//! crates add through [`router_with`] (`GET /api/zenith/sessions`), and the static SPA as
//! the fallback. The typed HTTP API, assets, uploads, OTLP, `/mcp` and the device hub
//! join the same router as their crates land.
//!
//! Middlewares, outermost first: CORS (preflights are answered before anything else),
//! gzip, then the command readiness gate (every request waits for startup to finish).

pub mod conformance;

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::compression::CompressionLayer;
use zc_http::embed::{embed_json, parent_origins_from_env, EMBED_CONFIG_PATH};
use zc_http::static_files::static_site;
use zc_http::ws::ws_upgrade;
use zc_http::{CorsPolicy, OriginCheck, ReadinessGate, StaticSite, WsAuthenticator, WsState};
use zc_rpc::RpcServer;

/// What the HTTP layer needs to know.
#[derive(Clone, Debug)]
pub struct HttpConfig {
    /// The built web app (`apps/server/dist/client`); `None` answers 503 for pages.
    pub static_dir: Option<PathBuf>,
    /// `ZENITH_CODE_PARENT_ORIGINS`, validated.
    pub parent_origins: Vec<String>,
    pub cors: CorsPolicy,
    /// Off by default (the TS server has no Origin check).
    pub ws_origin_check: Option<OriginCheck>,
}

impl HttpConfig {
    /// Parent origins from the environment, wildcard CORS, no Origin check.
    pub fn from_env(static_dir: Option<PathBuf>) -> Self {
        Self {
            static_dir,
            parent_origins: parent_origins_from_env(),
            cors: CorsPolicy::browser_api(),
            ws_origin_check: None,
        }
    }
}

/// Where the built web app is when no directory is given: `ZENITH_CODE_STATIC_DIR`, else
/// `code/apps/server/dist/client` in this repository if it exists.
pub fn default_static_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("ZENITH_CODE_STATIC_DIR") {
        return Some(dir.into());
    }
    let repo_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../code/apps/server/dist/client");
    repo_dir.is_dir().then_some(repo_dir)
}

/// The services the routes use.
#[derive(Clone)]
pub struct AppServices {
    pub rpc: Arc<RpcServer>,
    pub auth: Arc<dyn WsAuthenticator>,
    pub readiness: ReadinessGate,
}

/// The route table with its middlewares.
pub fn router(config: &HttpConfig, services: &AppServices) -> Router {
    router_with(config, services, Router::new())
}

/// [`router`] plus `api`: typed endpoints (auth, orchestration, …) that already carry their
/// state. They sit before the static fallback and behind the same middlewares.
pub fn router_with(config: &HttpConfig, services: &AppServices, api: Router) -> Router {
    router_with_fallback(config, services, api, Router::new())
}

/// [`router_with`] plus `fallback_api`: routes consulted only when nothing in `api` (or `/ws`,
/// `embed.json`) matches the path, before the static SPA. The server puts its placeholder
/// typed endpoints there, so a real route of the same path always wins without the two
/// routers having to know about each other.
pub fn router_with_fallback(config: &HttpConfig, services: &AppServices, api: Router, fallback_api: Router) -> Router {
    let site = Arc::new(StaticSite::new(config.static_dir.clone(), &config.parent_origins));
    let origins = Arc::new(config.parent_origins.clone());
    let ws_state = WsState {
        rpc: services.rpc.clone(),
        auth: services.auth.clone(),
        origin_check: config.ws_origin_check.clone(),
    };
    Router::new()
        .merge(api)
        .merge(Router::new().route("/ws", get(ws_upgrade)).with_state(ws_state))
        .merge(Router::new().route(EMBED_CONFIG_PATH, get(embed_json)).with_state(origins))
        .fallback_service(fallback_api.fallback_service(Router::new().fallback(static_site).with_state(site)))
        .layer(from_fn_with_state(services.readiness.clone(), zc_http::readiness::readiness))
        .layer(CompressionLayer::new().compress_when(EffectCompressible))
        .layer(from_fn_with_state(Arc::new(config.cors.clone()), zc_http::cors::cors))
}

/// Effect's `HttpMiddleware.compression` policy: text-like types only (`defaultCompressible`),
/// 1 KiB and up. Media must stay as it is: compressing a video drops its `Accept-Ranges` and
/// `Content-Length`, and the browser can no longer seek in it.
#[derive(Clone, Copy, Debug, Default)]
pub struct EffectCompressible;

impl tower_http::compression::Predicate for EffectCompressible {
    fn should_compress<B>(&self, response: &axum::http::Response<B>) -> bool
    where
        B: axum::body::HttpBody,
    {
        let compressible = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|content_type| {
                let essence = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
                essence.starts_with("text/")
                    || matches!(
                        essence.as_str(),
                        "application/json" | "application/javascript" | "application/xml" | "image/svg+xml" | "application/wasm"
                    )
                    || essence.ends_with("+json")
                    || essence.ends_with("+xml")
            });
        compressible && tower_http::compression::predicate::SizeAbove::new(1024).should_compress(response)
    }
}

/// Serves until `shutdown` resolves, then shuts down gracefully: stop accepting, close
/// every WebSocket (code 1001, requests interrupted) and let in-flight HTTP requests
/// finish.
pub async fn serve(listener: TcpListener, router: Router, rpc: Arc<RpcServer>, shutdown: impl Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    let stop = CancellationToken::new();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            shutdown.await;
            stop.cancel();
        }
    });
    // Upgraded sockets are not tracked by axum's graceful shutdown, so close them here.
    let sockets_closed = tokio::spawn({
        let stop = stop.clone();
        async move {
            stop.cancelled().await;
            rpc.shutdown().await;
        }
    });
    let result = axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown({
            let stop = stop.clone();
            async move { stop.cancelled().await }
        })
        .await;
    stop.cancel();
    let _ = sockets_closed.await;
    result
}

/// Resolves on Ctrl-C or SIGTERM (the dashboard sends SIGTERM, then SIGKILL after 5 s).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => futures::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = futures::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
