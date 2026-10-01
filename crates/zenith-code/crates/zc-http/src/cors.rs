//! CORS, as the TS server configures Effect's `HttpMiddleware.cors` (`http.ts`
//! `browserApiCorsLayer`, plan §2.6), reproduced header for header:
//!
//! - every response gets `access-control-allow-origin: *` (no credentials);
//! - `OPTIONS` is answered at once with 204, `access-control-allow-methods: GET, POST,
//!   OPTIONS`, `access-control-allow-headers: authorization,b3,traceparent,content-type,dpop`
//!   and `access-control-max-age: 600`;
//! - in dev (`--dev-url`), an allowlist of origins with `access-control-allow-credentials:
//!   true` instead of `*`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, StatusCode};

pub const ALLOWED_METHODS: &str = "GET, POST, OPTIONS";
pub const ALLOWED_HEADERS: &str = "authorization,b3,traceparent,content-type,dpop";
pub const MAX_AGE_SECONDS: u32 = 600;

/// Which origins get CORS headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CorsPolicy {
    /// Empty: `*`. One: always that origin. Several: the request's origin if listed.
    pub allowed_origins: Vec<String>,
    pub credentials: bool,
}

impl CorsPolicy {
    /// The default: any origin, no credentials.
    pub fn browser_api() -> Self {
        Self::default()
    }

    /// Dev mode: the dev server's origin and friends, with credentials.
    pub fn dev(allowed_origins: Vec<String>) -> Self {
        Self {
            allowed_origins,
            credentials: true,
        }
    }

    fn origin_headers(&self, request_origin: Option<&HeaderValue>, out: &mut Vec<(HeaderName, HeaderValue)>) {
        match self.allowed_origins.len() {
            0 => out.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"))),
            1 => {
                if let Ok(value) = HeaderValue::from_str(&self.allowed_origins[0]) {
                    out.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, value));
                }
                out.push((header::VARY, HeaderValue::from_static("Origin")));
            }
            _ => {
                let allowed = request_origin
                    .and_then(|o| o.to_str().ok())
                    .filter(|o| self.allowed_origins.iter().any(|a| a == o));
                if let (Some(_), Some(value)) = (allowed, request_origin) {
                    out.push((header::ACCESS_CONTROL_ALLOW_ORIGIN, value.clone()));
                }
                out.push((header::VARY, HeaderValue::from_static("Origin")));
            }
        }
        if self.credentials {
            out.push((header::ACCESS_CONTROL_ALLOW_CREDENTIALS, HeaderValue::from_static("true")));
        }
    }
}

/// Adds `value` to the response's `Vary` unless it is already there (Effect's `varyWith`).
fn merge_vary(headers: &mut HeaderMap, value: &str) {
    let existing: Vec<String> = headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .collect();
    if existing.iter().any(|v| v == "*" || v.eq_ignore_ascii_case(value)) {
        return;
    }
    let mut merged = existing;
    merged.push(value.to_owned());
    if let Ok(v) = HeaderValue::from_str(&merged.join(", ")) {
        headers.insert(header::VARY, v);
    }
}

/// The middleware: `axum::middleware::from_fn_with_state(Arc::new(policy), cors)`.
pub async fn cors(State(policy): State<Arc<CorsPolicy>>, request: Request, next: Next) -> Response {
    let origin = request.headers().get(header::ORIGIN).cloned();
    let mut extra = Vec::new();
    policy.origin_headers(origin.as_ref(), &mut extra);
    if request.method() == Method::OPTIONS {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        for (name, value) in extra {
            if name == header::VARY {
                merge_vary(headers, value.to_str().unwrap_or_default());
            } else {
                headers.insert(name, value);
            }
        }
        headers.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static(ALLOWED_METHODS));
        headers.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from(MAX_AGE_SECONDS));
        headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static(ALLOWED_HEADERS));
        return response;
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in extra {
        if name == header::VARY {
            merge_vary(headers, value.to_str().unwrap_or_default());
        } else {
            headers.insert(name, value);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_for(policy: &CorsPolicy, origin: Option<&str>) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let origin = origin.map(|o| HeaderValue::from_str(o).unwrap());
        policy.origin_headers(origin.as_ref(), &mut out);
        out.into_iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned())).collect()
    }

    #[test]
    fn wildcard_by_default() {
        assert_eq!(
            headers_for(&CorsPolicy::browser_api(), Some("http://evil.test")),
            vec![("access-control-allow-origin".into(), "*".into())]
        );
    }

    #[test]
    fn dev_allowlist_with_credentials() {
        let policy = CorsPolicy::dev(vec!["http://localhost:5173".into(), "app://t3".into()]);
        assert_eq!(
            headers_for(&policy, Some("http://localhost:5173")),
            vec![
                ("access-control-allow-origin".into(), "http://localhost:5173".into()),
                ("vary".into(), "Origin".into()),
                ("access-control-allow-credentials".into(), "true".into()),
            ]
        );
        assert_eq!(
            headers_for(&policy, Some("http://other.test")),
            vec![("vary".into(), "Origin".into()), ("access-control-allow-credentials".into(), "true".into())]
        );
    }

    #[test]
    fn vary_merges() {
        let mut h = HeaderMap::new();
        h.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        merge_vary(&mut h, "Origin");
        assert_eq!(h.get(header::VARY).unwrap(), "Accept-Encoding, Origin");
        merge_vary(&mut h, "origin");
        assert_eq!(h.get(header::VARY).unwrap(), "Accept-Encoding, Origin");
    }
}
