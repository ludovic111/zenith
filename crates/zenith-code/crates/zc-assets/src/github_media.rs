//! Media a pull request body points at on GitHub (`@t3tools/shared/githubMedia`,
//! `GitHubMediaFetch.ts`). A private repository answers an unauthenticated request for one with
//! 404, so the server fetches it with the `gh` credential and streams the bytes back.
//!
//! The credential rides only on requests to GitHub's own hosts: a redirect to a signed object
//! URL is followed here (at most 3 hops, `https` only) without it. Only pictures, recordings and
//! audio leave this origin; an SVG gets the document sandbox.

use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use futures::stream::BoxStream;
use futures::StreamExt as _;
use http::{HeaderMap, HeaderValue, StatusCode};
use regex::Regex;
use zc_auth::SharedClock;

use crate::preview::decode_uri_component;

const RAW_HOST: &str = "raw.githubusercontent.com";
const LFS_HOST: &str = "media.githubusercontent.com";
/// Exactly the hosts the credential is for.
const CREDENTIALED_HOSTS: &[&str] = &["github.com", "www.github.com", "raw.githubusercontent.com", "media.githubusercontent.com"];
const MAX_REDIRECTS: usize = 3;
const TOKEN_CACHE_TTL_MS: i64 = 5 * 60_000;
const TOKEN_CACHE_MAX_ENTRIES: usize = 32;
const FORWARDED_REQUEST_HEADERS: &[&str] = &["range", "if-range"];
const FORWARDED_RESPONSE_HEADERS: &[&str] = &["content-type", "content-length", "content-range", "accept-ranges", "etag", "last-modified"];
const SVG_CONTENT_TYPE: &str = "image/svg+xml";
const SVG_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// `canonicalUrl`: port, userinfo and fragment say nothing about which bytes GitHub serves.
fn canonical_url(host: &str, url: &url::Url) -> String {
    let search = url
        .query()
        .filter(|query| !query.is_empty())
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    format!("https://{host}{}{search}", url.path())
}

/// `githubMediaFetchUrl`: the URL to fetch with a GitHub credential, or `None` when `source`
/// is not GitHub-hosted media.
pub fn github_media_fetch_url(source: &str) -> Option<String> {
    static ATTACHMENT: OnceLock<Regex> = OnceLock::new();
    static LEGACY_ATTACHMENT: OnceLock<Regex> = OnceLock::new();
    static REPOSITORY_FILE: OnceLock<Regex> = OnceLock::new();
    let url = url::Url::parse(source).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?.to_lowercase();
    if host == RAW_HOST || host == LFS_HOST {
        return Some(canonical_url(&host, &url));
    }
    if host != "github.com" && host != "www.github.com" {
        return None;
    }
    let path = url.path();
    if regex(&ATTACHMENT, r"^/user-attachments/assets/[A-Za-z0-9_-]+$").is_match(path)
        || regex(&LEGACY_ATTACHMENT, r"^/[^/]+/[^/]+/assets/[0-9]+/[A-Za-z0-9_-]+$").is_match(path)
    {
        return Some(format!("https://github.com{path}"));
    }
    let captures = regex(
        &REPOSITORY_FILE,
        r"^/([^/]+)/([^/]+)/(?:raw|blob)/([^\n\r\x{2028}\x{2029}]*[^/\n\r\x{2028}\x{2029}])$",
    )
    .captures(path)?;
    Some(format!("https://{RAW_HOST}/{}/{}/{}", &captures[1], &captures[2], &captures[3]))
}

/// `githubMediaFileName`: the last path segment, decoded when it decodes, without control
/// characters or slashes; `github-media` when nothing is left.
pub fn github_media_file_name(fetch_url: &str) -> String {
    let segment = url::Url::parse(fetch_url)
        .ok()
        .map(|url| url.path().rsplit('/').next().unwrap_or("").to_owned())
        .unwrap_or_default();
    let decoded = decode_uri_component(&segment).unwrap_or(segment);
    let name: String = decoded.chars().filter(|c| !c.is_control() && *c != '\\' && *c != '/').collect();
    if name.is_empty() {
        "github-media".into()
    } else {
        name
    }
}

fn is_credentialed_host(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_lowercase))
        .is_some_and(|host| CREDENTIALED_HOSTS.contains(&host.as_str()))
}

/// `/^(?:image|video|audio)\/[\w!#$&^.+-]+$/i`.
fn is_media_content_type(value: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"(?i)^(?:image|video|audio)/[A-Za-z0-9_!#$&^.+-]+$").is_match(value)
}

/// One upstream answer.
pub struct UpstreamResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<bytes::Bytes, std::io::Error>>,
}

/// The HTTP client (injectable for tests): one GET, redirects not followed, no decoding.
#[async_trait]
pub trait MediaHttp: Send + Sync {
    async fn get(&self, url: &str, headers: Vec<(String, String)>) -> Result<UpstreamResponse, String>;
}

/// [`MediaHttp`] over reqwest.
pub struct ReqwestMediaHttp(reqwest::Client);

impl Default for ReqwestMediaHttp {
    fn default() -> Self {
        Self(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        )
    }
}

#[async_trait]
impl MediaHttp for ReqwestMediaHttp {
    async fn get(&self, url: &str, headers: Vec<(String, String)>) -> Result<UpstreamResponse, String> {
        let mut request = self.0.get(url);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(|error| error.to_string())?;
        Ok(UpstreamResponse {
            status: response.status().as_u16(),
            headers: response.headers().clone(),
            body: response.bytes_stream().map(|chunk| chunk.map_err(std::io::Error::other)).boxed(),
        })
    }
}

/// Where the `gh` token comes from (injectable for tests): `gh auth token --hostname <host>`
/// run in `cwd`; an empty answer means "not signed in".
#[async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self, cwd: &str, host: &str) -> String;
}

/// [`TokenSource`] running the `gh` CLI.
#[derive(Debug, Default)]
pub struct GhCliTokenSource;

#[async_trait]
impl TokenSource for GhCliTokenSource {
    async fn token(&self, cwd: &str, host: &str) -> String {
        let mut input = zc_core::ProcessRunInput::new("gh", ["auth", "token", "--hostname", host]);
        input.cwd = Some(cwd.into());
        input.env = Some(zc_core::process::EnvOverlay::from([("GH_DEBUG".to_owned(), Some(String::new()))]));
        input.timeout = Some(std::time::Duration::from_secs(30));
        match zc_core::run_process(input).await {
            Ok(output) if output.code == Some(0) => output.stdout.trim().to_owned(),
            _ => String::new(),
        }
    }
}

/// `GitHubMediaFetch`: the token cache and the proxy.
pub struct GitHubMediaFetch {
    http: Arc<dyn MediaHttp>,
    tokens: Arc<dyn TokenSource>,
    clock: SharedClock,
    /// Insertion-ordered `(host, fetched at, token)`.
    token_cache: Mutex<Vec<(String, i64, String)>>,
}

impl GitHubMediaFetch {
    pub fn new(http: Arc<dyn MediaHttp>, tokens: Arc<dyn TokenSource>, clock: SharedClock) -> Self {
        Self {
            http,
            tokens,
            clock,
            token_cache: Mutex::new(Vec::new()),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Vec<(String, i64, String)>> {
        self.token_cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `githubToken`: per host, held no longer than 5 minutes; a missing credential is not
    /// cached, so a login takes effect on the next request.
    async fn github_token(&self, cwd: &str, host: &str) -> Option<String> {
        let now = self.clock.now_millis();
        if let Some((_, at, token)) = self.cache().iter().find(|(key, _, _)| key == host) {
            if now - at < TOKEN_CACHE_TTL_MS {
                return Some(token.clone());
            }
        }
        let token = self.tokens.token(cwd, host).await;
        if token.is_empty() {
            return None;
        }
        let mut cache = self.cache();
        cache.retain(|(key, _, _)| key != host);
        if cache.len() >= TOKEN_CACHE_MAX_ENTRIES {
            cache.remove(0);
        }
        cache.push((host.to_owned(), now, token.clone()));
        Some(token)
    }

    /// `fetchFollowingRedirects`: `Ok(None)` for a redirect chain that is not GitHub answering
    /// with bytes.
    async fn fetch_following_redirects(&self, url: &str, headers: &[(String, String)], token: Option<&str>) -> Result<Option<UpstreamResponse>, String> {
        let mut target = url.to_owned();
        for hop in 0.. {
            let mut request_headers: Vec<(String, String)> = headers.to_vec();
            request_headers.push(("accept-encoding".into(), "identity".into()));
            if let Some(token) = token.filter(|_| is_credentialed_host(&target)) {
                request_headers.push(("authorization".into(), format!("Bearer {token}")));
            }
            let response = self.http.get(&target, request_headers).await?;
            if !(300..400).contains(&response.status) {
                return Ok(Some(response));
            }
            let Some(location) = response.headers.get(http::header::LOCATION).and_then(|value| value.to_str().ok()) else {
                return Ok(None);
            };
            if hop >= MAX_REDIRECTS {
                return Ok(None);
            }
            let Ok(next) = url::Url::parse(&target).and_then(|base| base.join(location)) else {
                return Ok(None);
            };
            if next.scheme() != "https" {
                return Ok(None);
            }
            target = next.to_string();
        }
        unreachable!("the redirect loop returns")
    }

    /// `githubMediaResponse`. `Err` is a failed fetch (the route answers 502).
    pub async fn response(&self, url: &str, cwd: &str, expires_at: i64, request_headers: &HeaderMap) -> Result<Response, String> {
        let token = self.github_token(cwd, "github.com").await;
        let forwarded: Vec<(String, String)> = FORWARDED_REQUEST_HEADERS
            .iter()
            .filter_map(|name| {
                request_headers
                    .get(*name)
                    .and_then(|value| value.to_str().ok())
                    .map(|value| ((*name).to_owned(), value.to_owned()))
            })
            .collect();
        let response = self.fetch_following_redirects(url, &forwarded, token.as_deref()).await?;
        let remaining_seconds = (expires_at - self.clock.now_millis()).div_euclid(1000);
        let mut headers = HeaderMap::new();
        let cache_control = if remaining_seconds > 0 {
            format!("private, max-age={remaining_seconds}")
        } else {
            "private, no-store".to_owned()
        };
        headers.insert(http::header::CACHE_CONTROL, HeaderValue::from_str(&cache_control).expect("ascii header"));
        headers.insert(http::header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
        let empty = |status: u16, headers: HeaderMap| {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            *response.headers_mut() = headers;
            response
        };
        let Some(response) = response else {
            return Ok(empty(502, headers));
        };
        if response.status >= 400 {
            return Ok(empty(if response.status >= 500 { 502 } else { response.status }, headers));
        }
        let upstream_type = response
            .headers
            .get(http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or("").trim().to_lowercase())
            .unwrap_or_default();
        let content_type = if is_media_content_type(&upstream_type) {
            upstream_type
        } else {
            mime_guess::from_path(github_media_file_name(url))
                .first()
                .map(|mime| mime.essence_str().to_lowercase())
                .unwrap_or_default()
        };
        if !is_media_content_type(&content_type) {
            return Ok(empty(415, headers));
        }
        for name in FORWARDED_RESPONSE_HEADERS {
            if let Some(value) = response.headers.get(*name) {
                headers.insert(*name, value.clone());
            }
        }
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_str(&content_type).expect("validated content type"),
        );
        if content_type == SVG_CONTENT_TYPE {
            headers.insert(http::header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(SVG_CONTENT_SECURITY_POLICY));
        }
        let mut out = Response::new(Body::from_stream(response.body));
        *out.status_mut() = StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK);
        *out.headers_mut() = headers;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn narrows_sources_to_github_media() {
        let attachment = "https://github.com/user-attachments/assets/1a1842fb-6383-492f-873c-57aa0033fa6c";
        assert_eq!(github_media_fetch_url(attachment).as_deref(), Some(attachment));
        assert_eq!(
            github_media_fetch_url("https://github.com/owner/repo/blob/main/docs/shot.png").as_deref(),
            Some("https://raw.githubusercontent.com/owner/repo/main/docs/shot.png")
        );
        assert_eq!(
            github_media_fetch_url("https://github.com/owner/repo/assets/45952064/1a1842fb").as_deref(),
            Some("https://github.com/owner/repo/assets/45952064/1a1842fb")
        );
        assert_eq!(
            github_media_fetch_url("https://media.githubusercontent.com/media/owner/repo/main/a.mp4").as_deref(),
            Some("https://media.githubusercontent.com/media/owner/repo/main/a.mp4")
        );
        assert_eq!(
            github_media_fetch_url("https://user:pw@raw.githubusercontent.com:8443/o/r/main/x.png?raw=1#frag").as_deref(),
            Some("https://raw.githubusercontent.com/o/r/main/x.png?raw=1")
        );
        for url in [
            "https://example.com/shot.png",
            "https://example.com/shot.png?token=private-media-token",
            "http://github.com/user-attachments/assets/1a1842fb",
            "https://github.com/owner/repo/pull/1",
            "https://github.com/owner/repo/blob/main/",
            "not a url",
        ] {
            assert_eq!(github_media_fetch_url(url), None, "{url}");
        }
    }

    #[test]
    fn names_media_by_their_last_segment() {
        assert_eq!(github_media_file_name("https://raw.githubusercontent.com/o/r/main/100%.png"), "100%.png");
        assert_eq!(github_media_file_name("https://raw.githubusercontent.com/o/r/main/a%20b.png"), "a b.png");
        assert_eq!(github_media_file_name("https://raw.githubusercontent.com/o/r/main/a%2Fb.png"), "ab.png");
        assert_eq!(github_media_file_name("https://github.com/"), "github-media");
    }

    struct CountingTokens(AtomicUsize);

    #[async_trait]
    impl TokenSource for CountingTokens {
        async fn token(&self, _cwd: &str, _host: &str) -> String {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                String::new()
            } else {
                "signed-in".into()
            }
        }
    }

    struct AuthorizationEcho(Mutex<Vec<Option<String>>>);

    #[async_trait]
    impl MediaHttp for AuthorizationEcho {
        async fn get(&self, _url: &str, headers: Vec<(String, String)>) -> Result<UpstreamResponse, String> {
            let authorization = headers.iter().find(|(name, _)| name == "authorization").map(|(_, value)| value.clone());
            let status = if authorization.is_some() { 200 } else { 404 };
            self.0.lock().unwrap().push(authorization);
            let mut headers = HeaderMap::new();
            headers.insert(http::header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            Ok(UpstreamResponse {
                status,
                headers,
                body: futures::stream::empty().boxed(),
            })
        }
    }

    /// `loads private media immediately after login and reuses the found credential`.
    #[tokio::test]
    async fn loads_private_media_right_after_login_and_reuses_the_credential() {
        let tokens = Arc::new(CountingTokens(AtomicUsize::new(0)));
        let http = Arc::new(AuthorizationEcho(Mutex::new(Vec::new())));
        let fetch = GitHubMediaFetch::new(http.clone(), tokens.clone(), zc_auth::system_clock());
        let url = "https://raw.githubusercontent.com/owner/repo/main/shot.png";
        let headers = HeaderMap::new();
        assert_eq!(fetch.response(url, "/repo", i64::MAX / 2, &headers).await.unwrap().status(), 404);
        assert_eq!(fetch.response(url, "/repo", i64::MAX / 2, &headers).await.unwrap().status(), 200);
        assert_eq!(fetch.response(url, "/repo", i64::MAX / 2, &headers).await.unwrap().status(), 200);
        assert_eq!(tokens.0.load(Ordering::SeqCst), 2);
        assert_eq!(
            *http.0.lock().unwrap(),
            vec![None, Some("Bearer signed-in".into()), Some("Bearer signed-in".into())]
        );
    }
}
