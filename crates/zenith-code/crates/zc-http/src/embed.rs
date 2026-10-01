//! zenith embedding (`apps/server/src/zenith/embed.ts`, plan §2.7).
//!
//! zenith shows zenith code in an iframe. The parent origins allowed to frame it come
//! from `ZENITH_CODE_PARENT_ORIGINS`, a comma-separated list of http(s) origins.

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::{header, HeaderValue};
use std::sync::Arc;

pub const EMBED_CONFIG_PATH: &str = "/zenith/embed.json";
pub const PARENT_ORIGINS_ENV: &str = "ZENITH_CODE_PARENT_ORIGINS";
pub const DEFAULT_PARENT_ORIGINS: &str = "http://127.0.0.1:4747,http://127.0.0.1:4748";

/// Validated origins: an entry is kept only if it is an http(s) URL whose origin is the
/// entry itself (so no path, no trailing slash, no default port); duplicates are
/// dropped, order kept. `None` means the variable is unset (the default applies); an
/// empty string means no parent at all.
pub fn parse_parent_origins(raw: Option<&str>) -> Vec<String> {
    let mut origins: Vec<String> = Vec::new();
    for entry in raw.unwrap_or(DEFAULT_PARENT_ORIGINS).split(',') {
        let value = entry.trim();
        if value.is_empty() {
            continue;
        }
        let Ok(url) = url::Url::parse(value) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        let origin = url.origin().ascii_serialization();
        if origin == value && !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    origins
}

/// The parent origins from the environment.
pub fn parent_origins_from_env() -> Vec<String> {
    parse_parent_origins(std::env::var(PARENT_ORIGINS_ENV).ok().as_deref())
}

/// `frame-ancestors 'self' <origins…>`, for HTML documents only.
pub fn frame_ancestors_policy(origins: &[String]) -> String {
    let mut parts = vec!["frame-ancestors", "'self'"];
    parts.extend(origins.iter().map(String::as_str));
    parts.join(" ")
}

/// `GET /zenith/embed.json`: public, `{"parentOrigins":[…]}`, `cache-control: no-store`.
/// Also the dashboard's liveness probe.
pub async fn embed_json(State(origins): State<Arc<Vec<String>>>) -> Response {
    embed_json_response(&origins)
}

pub fn embed_json_response(origins: &[String]) -> Response {
    let body = serde_json::json!({ "parentOrigins": origins }).to_string();
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/json")),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_origins() {
        assert_eq!(
            parse_parent_origins(None),
            vec!["http://127.0.0.1:4747".to_string(), "http://127.0.0.1:4748".to_string()]
        );
        assert!(parse_parent_origins(Some("")).is_empty());
    }

    #[test]
    fn validation_matches_embed_ts() {
        let raw = " http://localhost:3000 ,https://a.example,http://localhost:3000,\
                   http://x.test/,ftp://x.test,not a url,http://x.test:80,HTTP://Y.TEST,https://[::1]:9";
        assert_eq!(
            parse_parent_origins(Some(raw)),
            vec![
                "http://localhost:3000".to_string(),
                "https://a.example".to_string(),
                "https://[::1]:9".to_string()
            ]
        );
    }

    #[test]
    fn policy() {
        assert_eq!(frame_ancestors_policy(&[]), "frame-ancestors 'self'");
        assert_eq!(
            frame_ancestors_policy(&parse_parent_origins(None)),
            "frame-ancestors 'self' http://127.0.0.1:4747 http://127.0.0.1:4748"
        );
    }
}
