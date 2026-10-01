//! The SPA from `apps/server/dist/client` (`http.ts` `handleStaticAndDevRequest`, plan
//! §1.2 last row):
//!
//! - `/` is `index.html`; an extensionless path resolves to `<dir>/index.html`; anything
//!   missing falls back to the root `index.html` (404 only if that is missing too);
//! - traversal (`..` before or after normalization, NUL) is a 400;
//! - hashed `assets/*-xxxxxxxx.*` files listed in `.vite/manifest.json` are cached as
//!   immutable, everything else is `no-cache`;
//! - non-HTML gets a weak ETag and Last-Modified and may answer 304; HTML never does;
//! - HTML gets `Content-Security-Policy: frame-ancestors 'self' <parent origins>`, and is
//!   the only response that does.
//!
//! Like the TS server, the path is used as it appears in the URL (not percent-decoded).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use regex::Regex;
use tokio_util::io::ReaderStream;

use crate::embed::frame_ancestors_policy;

static IMMUTABLE_ASSET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^assets/.+-[\w-]{8}\.[^/]+$").expect("valid regex"));

/// A static site rooted at a directory.
#[derive(Debug)]
pub struct StaticSite {
    root: Option<PathBuf>,
    immutable: HashSet<String>,
    csp: HeaderValue,
}

impl StaticSite {
    /// Reads `.vite/manifest.json` once (a missing or bad manifest just means no
    /// immutable caching). `None` answers every request with 503, like a server started
    /// with no static directory.
    pub fn new(root: Option<PathBuf>, parent_origins: &[String]) -> Self {
        let root = root.map(|r| std::path::absolute(&r).unwrap_or(r));
        let immutable = root.as_deref().map(load_manifest).unwrap_or_default();
        let csp = HeaderValue::from_str(&frame_ancestors_policy(parent_origins)).unwrap_or_else(|_| HeaderValue::from_static("frame-ancestors 'self'"));
        Self { root, immutable, csp }
    }

    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    pub async fn serve(&self, method: &Method, path: &str, request_headers: &HeaderMap) -> Response {
        if method != Method::GET && method != Method::HEAD {
            return StatusCode::NOT_FOUND.into_response();
        }
        let Some(root) = &self.root else {
            return text(StatusCode::SERVICE_UNAVAILABLE, "No static directory configured and no dev URL set.");
        };
        let request_path = if path == "/" { "/index.html" } else { path };
        let raw_relative = request_path.trim_start_matches(['/', '\\']);
        let relative = normalize(raw_relative);
        let relative = relative.trim_start_matches(['/', '\\']);
        if relative.is_empty() || raw_relative.starts_with("..") || relative.starts_with("..") || relative.contains('\0') {
            return text(StatusCode::BAD_REQUEST, "Invalid static file path");
        }
        let mut file_path = if relative == "." { root.clone() } else { root.join(relative) };
        if !file_path.starts_with(root) {
            return text(StatusCode::BAD_REQUEST, "Invalid static file path");
        }
        if extname(relative).is_empty() {
            file_path = file_path.join("index.html");
        }

        let (file_path, file, meta) = match open_file(&file_path).await {
            Some((file, meta)) => (file_path, file, meta),
            None => {
                let index = root.join("index.html");
                match open_file(&index).await {
                    Some((file, meta)) => (index, file, meta),
                    None => return text(StatusCode::NOT_FOUND, "Not Found"),
                }
            }
        };

        let mime = mime_guess::from_path(&file_path).first_or_octet_stream();
        let is_html = mime.essence_str() == "text/html";
        let relative_path = file_path.strip_prefix(root).map(|p| p.to_string_lossy().replace('\\', "/")).unwrap_or_default();
        let immutable = !is_html && IMMUTABLE_ASSET.is_match(&relative_path) && self.immutable.contains(&relative_path);

        let mut headers = HeaderMap::new();
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if immutable { "public, max-age=31536000, immutable" } else { "no-cache" }),
        );
        if is_html {
            headers.insert(header::CONTENT_SECURITY_POLICY, self.csp.clone());
        }
        // Deployments can keep an HTML file's size and mtime while changing its bundle URLs.
        let modified = if is_html { None } else { meta.modified().ok() };
        let etag = modified.map(|m| format!("W/\"{:x}-{:x}\"", meta.len(), millis(m)));
        if let (Some(etag), Some(modified)) = (&etag, modified) {
            if let Ok(v) = HeaderValue::from_str(etag) {
                headers.insert(header::ETAG, v);
            }
            if let Ok(v) = HeaderValue::from_str(&httpdate::fmt_http_date(modified)) {
                headers.insert(header::LAST_MODIFIED, v);
            }
        }

        if !is_html && unchanged(request_headers, etag.as_deref(), modified) {
            headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
            let mut response = StatusCode::NOT_MODIFIED.into_response();
            response.headers_mut().extend(headers);
            return response;
        }

        let content_type = if is_html { "text/html; charset=utf-8".to_owned() } else { mime.to_string() };
        if let Ok(v) = HeaderValue::from_str(&content_type) {
            headers.insert(header::CONTENT_TYPE, v);
        }
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(meta.len()));
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from_stream(ReaderStream::new(file))
        };
        let mut response = Response::new(body);
        response.headers_mut().extend(headers);
        response
    }
}

/// The fallback handler: `Router::fallback(static_site)` with `State<Arc<StaticSite>>`.
pub async fn static_site(State(site): State<Arc<StaticSite>>, request: Request) -> Response {
    site.serve(request.method(), request.uri().path(), request.headers()).await
}

fn text(status: StatusCode, body: &'static str) -> Response {
    (status, [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"))], body).into_response()
}

async fn open_file(path: &Path) -> Option<(tokio::fs::File, std::fs::Metadata)> {
    let meta = tokio::fs::metadata(path).await.ok()?;
    if !meta.is_file() {
        return None;
    }
    let file = tokio::fs::File::open(path).await.ok()?;
    let meta = file.metadata().await.ok()?;
    meta.is_file().then_some((file, meta))
}

fn millis(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// `If-None-Match` wins over `If-Modified-Since`; weak comparison.
fn unchanged(request: &HeaderMap, etag: Option<&str>, modified: Option<SystemTime>) -> bool {
    if let Some(inm) = request.get(header::IF_NONE_MATCH) {
        let Ok(inm) = inm.to_str() else { return false };
        return inm.split(',').any(|candidate| {
            let candidate = candidate.trim();
            if candidate == "*" {
                return true;
            }
            let Some(etag) = etag else { return false };
            let strong = candidate.strip_prefix("W/").or_else(|| candidate.strip_prefix("w/")).unwrap_or(candidate);
            strong == &etag[2..]
        });
    }
    let (Some(ims), Some(modified)) = (request.get(header::IF_MODIFIED_SINCE), modified) else {
        return false;
    };
    let Some(since) = ims.to_str().ok().and_then(|s| httpdate::parse_http_date(s).ok()) else {
        return false;
    };
    // Compared at second precision, like `Date.parse(modifiedAt.toUTCString())`.
    let modified_secs = modified.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let since_secs = since.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    modified_secs <= since_secs
}

/// Node's `path.posix.normalize` for a relative path: `.` and empty segments go, `..`
/// eats the previous segment or stays at the front; `""` becomes `"."`.
fn normalize(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if matches!(out.last(), Some(last) if *last != "..") {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let mut joined = out.join("/");
    if joined.is_empty() {
        joined.push('.');
    } else if path.ends_with('/') {
        joined.push('/');
    }
    joined
}

/// Node's `path.extname` on the last segment.
fn extname(path: &str) -> &str {
    let base = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
    match base.rfind('.') {
        Some(0) | None => "",
        Some(i) => &base[i..],
    }
}

fn load_manifest(root: &Path) -> HashSet<String> {
    let Ok(text) = std::fs::read_to_string(root.join(".vite").join("manifest.json")) else {
        return HashSet::new();
    };
    let Ok(serde_json::Value::Object(entries)) = serde_json::from_str::<serde_json::Value>(&text) else {
        return HashSet::new();
    };
    let mut files = HashSet::new();
    for entry in entries.values() {
        let Some(file) = entry.get("file").and_then(|f| f.as_str()) else {
            // The TS schema rejects the whole manifest if an entry has no file.
            return HashSet::new();
        };
        files.insert(file.to_owned());
        for key in ["css", "assets"] {
            if let Some(list) = entry.get(key).and_then(|l| l.as_array()) {
                files.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_owned));
            }
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_like_node() {
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("a/./b//c"), "a/b/c");
        assert_eq!(normalize("a/../../b"), "../b");
        assert_eq!(normalize("a/b/../c/"), "a/c/");
        assert_eq!(normalize("assets/x.js"), "assets/x.js");
    }

    #[test]
    fn extname_like_node() {
        assert_eq!(extname("a/b.js"), ".js");
        assert_eq!(extname("a/.well-known"), "");
        assert_eq!(extname("a/b"), "");
        assert_eq!(extname("a.b/c"), "");
        assert_eq!(extname("x."), ".");
    }

    #[test]
    fn immutable_pattern() {
        assert!(IMMUTABLE_ASSET.is_match("assets/index-AbCd_1-2.js"));
        assert!(!IMMUTABLE_ASSET.is_match("assets/index.js"));
        assert!(!IMMUTABLE_ASSET.is_match("other/index-AbCd1234.js"));
    }
}
