//! The raw routes (`http.ts`):
//!
//! - `GET|HEAD /api/assets/<token>/<name>`: what a signed asset URL grants, with the response
//!   policy of `assetResponseHeaders` (downloads sandboxed, inline media and documents only for
//!   safe types, `sandbox` CSP for HTML and SVG), single byte ranges for audio and video, and
//!   GitHub media proxied.
//! - `POST /api/attachments/upload/<token>`: a signed upload; `Content-Length` (when sent) must
//!   equal the signed size. Answers 204.
//!
//! Neither route authenticates: the signed token is the capability.

use std::io::SeekFrom;
use std::sync::{Arc, OnceLock};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::Request;
use axum::response::{IntoResponse, Response};
use axum::routing::{on, MethodFilter};
use axum::Router;
use futures::StreamExt as _;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use regex::Regex;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};
use tokio_util::io::ReaderStream;

use crate::access::{ResolvedAsset, ResolvedFile, ASSET_ROUTE_PREFIX};
use crate::preview::encode_uri_component;
use crate::upload::{StoreUploadResult, ATTACHMENT_UPLOAD_ROUTE_PREFIX};
use crate::Assets;

const SVG_CONTENT_SECURITY_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";
/// HTML previews are agent output: scripts run in an opaque origin, away from cookies, storage
/// and the API.
const HTML_CONTENT_SECURITY_POLICY: &str = "sandbox allow-scripts allow-forms allow-popups allow-modals";

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// `DOWNLOAD_MIME_TYPE_PATTERN`.
fn is_mime_shaped(value: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"^[A-Za-z0-9_!#$&^.+-]+/[A-Za-z0-9_!#$&^.+-]+$").is_match(value)
}

/// `isSafeDownloadMimeType`: never a type a browser may render as a document.
fn is_safe_download_mime_type(value: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    is_mime_shaped(value) && !regex(&RE, r"(?:^text/html$|/xml(?:$|-)|\+xml$)").is_match(&value.trim().to_lowercase())
}

/// `isSafeInlineMediaMimeType`.
fn is_safe_inline_media_mime_type(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    is_mime_shaped(value) && (lower.starts_with("audio/") || lower.starts_with("video/"))
}

/// `isSafeInlineDocumentMimeType`.
fn is_safe_inline_document_mime_type(value: &str) -> bool {
    let lower = value.to_lowercase();
    lower == "application/pdf" || lower == "text/html"
}

/// `downloadContentDisposition`: RFC 6266 with an ASCII fallback name plus a UTF-8 `filename*`.
pub fn download_content_disposition(file_name: Option<&str>) -> String {
    let Some(file_name) = file_name else {
        return "attachment".into();
    };
    let sanitized: String = file_name
        .chars()
        .map(|c| if c.is_control() || c == '"' || c == '\\' { '_' } else { c })
        .collect();
    // Per UTF-16 code unit, like the JS regex: an astral character becomes two underscores.
    let ascii_fallback: String = sanitized
        .chars()
        .flat_map(|c| if (' '..='~').contains(&c) { vec![c] } else { vec!['_'; c.len_utf16()] })
        .collect();
    if ascii_fallback == sanitized {
        return format!("attachment; filename=\"{ascii_fallback}\"");
    }
    let extended = encode_uri_component(&sanitized)
        .replace('\'', "%27")
        .replace('(', "%28")
        .replace(')', "%29")
        .replace('*', "%2A");
    format!("attachment; filename=\"{ascii_fallback}\"; filename*=UTF-8''{extended}")
}

/// The response policy of a served file (`assetResponseHeaders`), as ordered `(name, value)`
/// pairs (a later pair replaces an earlier one of the same name).
pub fn asset_response_headers(file_path: &str, download: bool, file_name: Option<&str>, mime_type: Option<&str>) -> Vec<(&'static str, String)> {
    let lower_path = file_path.to_lowercase();
    let inline_mime_type = mime_type.map(|mime| mime.split(';').next().unwrap_or("").trim().to_owned());
    let mut headers: Vec<(&'static str, String)> = vec![("cache-control", "private, max-age=3600".into()), ("x-content-type-options", "nosniff".into())];
    let mut set = |name: &'static str, value: String| {
        headers.retain(|(existing, _)| *existing != name);
        headers.push((name, value));
    };
    if download {
        set("content-disposition", download_content_disposition(file_name));
        set("content-security-policy", "default-src 'none'; sandbox".into());
        set(
            "content-type",
            match mime_type {
                Some(mime) if is_safe_download_mime_type(mime) => mime.to_owned(),
                _ => "application/octet-stream".into(),
            },
        );
    } else if let Some(inline) = inline_mime_type.as_deref().filter(|inline| is_safe_inline_media_mime_type(inline)) {
        set("content-type", inline.to_owned());
    } else if let Some(inline) = inline_mime_type.as_deref().filter(|inline| is_safe_inline_document_mime_type(inline)) {
        if inline.eq_ignore_ascii_case("text/html") {
            set("content-type", "text/html; charset=utf-8".into());
            set("content-security-policy", HTML_CONTENT_SECURITY_POLICY.into());
        } else {
            set("content-type", "application/pdf".into());
        }
    } else if lower_path.ends_with(".html") || lower_path.ends_with(".htm") {
        set("content-type", "text/html; charset=utf-8".into());
        set("content-security-policy", HTML_CONTENT_SECURITY_POLICY.into());
    }
    if !download && lower_path.ends_with(".svg") {
        set("content-security-policy", SVG_CONTENT_SECURITY_POLICY.into());
    }
    headers
}

/// `assetByteRange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteRange {
    Unsatisfiable,
    Range { offset: u64, bytes_to_read: u64, content_range: String },
}

/// One `bytes=first-last` range; unsupported syntax serves the whole file (`None`).
pub fn asset_byte_range(header: &str, size: u64) -> Option<ByteRange> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let captures = regex(&RE, r"(?i)^bytes=([0-9]*)-([0-9]*)$").captures(header.trim())?;
    let parse = |text: &str| -> Option<u128> { (!text.is_empty()).then(|| text.parse::<u128>().unwrap_or(u128::MAX)) };
    let first = parse(&captures[1]);
    let last = parse(&captures[2]);
    if first.is_none() && last.is_none() {
        return None;
    }
    if let (Some(first), Some(last)) = (first, last) {
        if last < first {
            return None;
        }
    }
    let size_wide = u128::from(size);
    if size == 0 || first.is_some_and(|first| first >= size_wide) || (first.is_none() && last == Some(0)) {
        return Some(ByteRange::Unsatisfiable);
    }
    let start = match (first, last) {
        (Some(first), _) => first,
        (None, Some(last)) if last >= size_wide => 0,
        (None, Some(last)) => size_wide - last,
        (None, None) => unreachable!(),
    };
    let end = match (first, last) {
        (Some(_), Some(last)) if last < size_wide => last,
        _ => size_wide - 1,
    };
    const MAX_SAFE_INTEGER: u128 = (1 << 53) - 1;
    if start > MAX_SAFE_INTEGER || end > MAX_SAFE_INTEGER {
        return Some(ByteRange::Unsatisfiable);
    }
    Some(ByteRange::Range {
        offset: start as u64,
        bytes_to_read: (end - start + 1) as u64,
        content_range: format!("bytes {start}-{end}/{size}"),
    })
}

fn text_response(status: StatusCode, body: &'static str) -> Response {
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    response
}

fn put_headers(response_headers: &mut HeaderMap, headers: &[(&'static str, String)]) {
    for (name, value) in headers {
        if let Ok(value) = HeaderValue::from_str(value) {
            response_headers.insert(HeaderName::from_static(name), value);
        }
    }
}

fn set(headers: &mut Vec<(&'static str, String)>, name: &'static str, value: String) {
    headers.retain(|(existing, _)| *existing != name);
    headers.push((name, value));
}

fn get<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(existing, _)| *existing == name).map(|(_, value)| value.as_str())
}

fn mtime_millis(metadata: &std::fs::Metadata) -> Option<i64> {
    let modified = metadata.modified().ok()?;
    Some(match modified.duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_millis() as i64,
        Err(before) => -(before.duration().as_millis() as i64),
    })
}

/// `W/"<size hex>-<mtime ms hex>"` and the HTTP date of the modification time.
fn validators(metadata: &std::fs::Metadata) -> (String, Option<String>) {
    let mtime = mtime_millis(metadata).unwrap_or(0);
    let etag = format!("W/\"{:x}-{:x}\"", metadata.len(), mtime);
    let last_modified = metadata.modified().ok().map(httpdate::fmt_http_date);
    (etag, last_modified)
}

fn guess_content_type(path: &str) -> String {
    mime_guess::from_path(path).first_raw().unwrap_or("application/octet-stream").to_owned()
}

async fn file_body(file: std::fs::File, offset: u64, length: u64) -> std::io::Result<Body> {
    let mut file = tokio::fs::File::from_std(file);
    file.seek(SeekFrom::Start(offset)).await?;
    Ok(Body::from_stream(ReaderStream::new(file.take(length))))
}

/// `assetFileResponse`.
pub async fn asset_file_response(asset: ResolvedFile, range: Option<&str>, if_range: Option<&str>, method: &Method) -> std::io::Result<Response> {
    let mut headers = asset_response_headers(&asset.path, asset.download, asset.file_name.as_deref(), asset.mime_type.as_deref());
    let media_info = match &asset.file {
        Some(file) => Some(file.stat()?),
        None => None,
    };
    let is_media = get(&headers, "content-type").is_some_and(|value| {
        let lower = value.to_ascii_lowercase();
        lower.starts_with("audio/") || lower.starts_with("video/")
    });
    let mut status = StatusCode::OK;
    let mut offset = 0u64;
    let mut bytes_to_read: Option<u64> = None;
    if is_media {
        // Host media can change in place, and attachment media must not outlive its URL.
        set(&mut headers, "cache-control", "private, no-store".into());
        set(&mut headers, "accept-ranges", "bytes".into());
        if let (true, Some(range), None) = (method == Method::GET, range, if_range) {
            let size = match &media_info {
                Some(info) => info.len(),
                None => std::fs::metadata(&asset.path)?.len(),
            };
            match asset_byte_range(range, size) {
                Some(ByteRange::Unsatisfiable) => {
                    set(&mut headers, "content-range", format!("bytes */{size}"));
                    let mut response = Response::new(Body::empty());
                    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                    put_headers(response.headers_mut(), &headers);
                    return Ok(response);
                }
                Some(ByteRange::Range {
                    offset: start,
                    bytes_to_read: count,
                    content_range,
                }) => {
                    status = StatusCode::PARTIAL_CONTENT;
                    offset = start;
                    bytes_to_read = Some(count);
                    set(&mut headers, "content-range", content_range);
                }
                None => {}
            }
        }
    }

    let (file, info) = match (asset.file, media_info) {
        (Some(media), Some(info)) => {
            if get(&headers, "content-type").is_none() {
                set(&mut headers, "content-type", guess_content_type(&asset.path));
            }
            if !is_media {
                let (etag, last_modified) = validators(&info);
                if let Some(last_modified) = last_modified {
                    set(&mut headers, "last-modified", last_modified);
                }
                set(&mut headers, "etag", etag);
            }
            (media.file, info)
        }
        _ => {
            // `HttpServerResponse.file`: opened by path now, with weak validators.
            let file = std::fs::File::open(&asset.path)?;
            let info = file.metadata()?;
            let (etag, last_modified) = validators(&info);
            set(&mut headers, "etag", etag);
            if let Some(last_modified) = last_modified {
                set(&mut headers, "last-modified", last_modified);
            }
            if get(&headers, "content-type").is_none() {
                set(&mut headers, "content-type", guess_content_type(&asset.path));
            }
            (file, info)
        }
    };
    let offset = offset.min(info.len());
    let available = info.len() - offset;
    let length = bytes_to_read.map_or(available, |count| count.min(available));
    set(&mut headers, "content-length", length.to_string());
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        file_body(file, offset, length).await?
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    put_headers(response.headers_mut(), &headers);
    Ok(response)
}

fn route_suffix<'a>(request: &'a Request, prefix: &str) -> Option<&'a str> {
    request.uri().path().strip_prefix(prefix)?.strip_prefix('/')
}

async fn serve_asset(assets: Arc<Assets>, request: Request) -> Response {
    let Some(suffix) = route_suffix(&request, ASSET_ROUTE_PREFIX) else {
        return text_response(StatusCode::NOT_FOUND, "Not Found");
    };
    let Some(separator) = suffix.find('/').filter(|index| *index > 0) else {
        return text_response(StatusCode::NOT_FOUND, "Not Found");
    };
    let (token, name) = (&suffix[..separator], &suffix[separator + 1..]);
    let Some(asset) = assets.access.resolve_asset(token, name).await else {
        return text_response(StatusCode::NOT_FOUND, "Not Found");
    };
    let method = request.method().clone();
    let headers = request.headers();
    match asset {
        ResolvedAsset::GithubMedia { url, cwd, expires_at } => match assets.github.response(&url, &cwd, expires_at as i64, headers).await {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(url, %error, "Failed to fetch GitHub media.");
                let mut response = Response::new(Body::empty());
                *response.status_mut() = StatusCode::BAD_GATEWAY;
                response
                    .headers_mut()
                    .insert(http::header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
                response
                    .headers_mut()
                    .insert(http::header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
                response
            }
        },
        ResolvedAsset::File(file) => {
            let header = |name: http::HeaderName| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_owned);
            let range = if method == Method::GET { header(http::header::RANGE) } else { None };
            let if_range = header(http::header::IF_RANGE);
            match asset_file_response(file, range.as_deref(), if_range.as_deref(), &method).await {
                Ok(response) => response,
                Err(error) => {
                    tracing::warn!(%error, "Failed to serve an asset.");
                    text_response(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
                }
            }
        }
    }
}

/// `Number(header)` is an integer equal to the signed size (`Number("")` is 0).
fn content_length_matches(header: &str, size_bytes: f64) -> bool {
    let trimmed = header.trim();
    let value = if trimmed.is_empty() { Some(0.0) } else { trimmed.parse::<f64>().ok() };
    value.is_some_and(|value| value.fract() == 0.0 && value == size_bytes)
}

async fn upload_attachment(assets: Arc<Assets>, request: Request) -> Response {
    let token = route_suffix(&request, ATTACHMENT_UPLOAD_ROUTE_PREFIX).unwrap_or("").to_owned();
    if token.is_empty() {
        return text_response(StatusCode::NOT_FOUND, "Not Found");
    }
    let Some(claims) = assets.uploads.validate_upload_token(&token).await else {
        return text_response(StatusCode::NOT_FOUND, "Not Found");
    };
    if let Some(content_length) = request.headers().get(http::header::CONTENT_LENGTH) {
        let text = String::from_utf8_lossy(content_length.as_bytes()).into_owned();
        if !content_length_matches(&text, claims.size_bytes) {
            return text_response(StatusCode::BAD_REQUEST, "Content-Length must match the upload size.");
        }
    }
    let body = request.into_body().into_data_stream().boxed();
    match assets.uploads.store_upload(&claims, body).await {
        StoreUploadResult::Ok => StatusCode::NO_CONTENT.into_response(),
        StoreUploadResult::Rejected { status, detail } => {
            let mut response = (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), detail).into_response();
            response
                .headers_mut()
                .insert(http::header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
            response
        }
    }
}

/// The two routes.
pub fn routes(assets: Arc<Assets>) -> Router {
    let asset_state = assets.clone();
    let upload_state = assets;
    Router::new()
        .route(
            &format!("{ASSET_ROUTE_PREFIX}/{{*rest}}"),
            on(MethodFilter::GET.or(MethodFilter::HEAD), move |request: Request| {
                serve_asset(asset_state.clone(), request)
            }),
        )
        .route(
            &format!("{ATTACHMENT_UPLOAD_ROUTE_PREFIX}/{{*rest}}"),
            on(MethodFilter::POST, move |request: Request| upload_attachment(upload_state.clone(), request)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_rfc_6266_dispositions() {
        assert_eq!(download_content_disposition(None), "attachment");
        assert_eq!(download_content_disposition(Some("report.pdf")), "attachment; filename=\"report.pdf\"");
        assert_eq!(download_content_disposition(Some("a\"b\\c\u{1}.txt")), "attachment; filename=\"a_b_c_.txt\"");
        assert_eq!(
            download_content_disposition(Some("résumé (1).pdf")),
            "attachment; filename=\"r_sum_ (1).pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9%20%281%29.pdf"
        );
        assert_eq!(
            download_content_disposition(Some("😀.png")),
            "attachment; filename=\"__.png\"; filename*=UTF-8''%F0%9F%98%80.png"
        );
    }

    fn header<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        get(headers, name)
    }

    #[test]
    fn applies_the_response_policy() {
        let download = asset_response_headers("/a/x.bin", true, Some("x.html"), Some("text/html"));
        assert_eq!(header(&download, "content-type"), Some("application/octet-stream"));
        assert_eq!(header(&download, "content-security-policy"), Some("default-src 'none'; sandbox"));
        let pdf = asset_response_headers("/a/x.pdf", true, Some("report.pdf"), Some("application/pdf"));
        assert_eq!(header(&pdf, "content-type"), Some("application/pdf"));
        let svg_download = asset_response_headers("/a/x.svg", true, None, Some("image/svg+xml"));
        assert_eq!(header(&svg_download, "content-type"), Some("application/octet-stream"));
        let video = asset_response_headers("/a/x.mp4", false, None, Some("video/mp4"));
        assert_eq!(header(&video, "content-type"), Some("video/mp4"));
        let html = asset_response_headers("/a/report.HTML", false, None, None);
        assert_eq!(header(&html, "content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(header(&html, "content-security-policy"), Some(HTML_CONTENT_SECURITY_POLICY));
        let svg = asset_response_headers("/a/icon.svg", false, None, None);
        assert_eq!(header(&svg, "content-type"), None);
        assert_eq!(header(&svg, "content-security-policy"), Some(SVG_CONTENT_SECURITY_POLICY));
        assert_eq!(header(&svg, "cache-control"), Some("private, max-age=3600"));
        assert_eq!(header(&svg, "x-content-type-options"), Some("nosniff"));
    }

    #[test]
    fn parses_single_byte_ranges() {
        let range = |offset, bytes_to_read, content_range: &str| {
            Some(ByteRange::Range {
                offset,
                bytes_to_read,
                content_range: content_range.into(),
            })
        };
        assert_eq!(asset_byte_range("bytes=2-5", 10), range(2, 4, "bytes 2-5/10"));
        assert_eq!(asset_byte_range("bytes=2-", 10), range(2, 8, "bytes 2-9/10"));
        assert_eq!(asset_byte_range("bytes=-3", 10), range(7, 3, "bytes 7-9/10"));
        assert_eq!(asset_byte_range("bytes=-30", 10), range(0, 10, "bytes 0-9/10"));
        assert_eq!(asset_byte_range("BYTES=0-99", 10), range(0, 10, "bytes 0-9/10"));
        assert_eq!(asset_byte_range("bytes=10-", 10), Some(ByteRange::Unsatisfiable));
        assert_eq!(asset_byte_range("bytes=-0", 10), Some(ByteRange::Unsatisfiable));
        assert_eq!(asset_byte_range("bytes=0-1", 0), Some(ByteRange::Unsatisfiable));
        assert_eq!(asset_byte_range("bytes=5-2", 10), None);
        assert_eq!(asset_byte_range("bytes=-", 10), None);
        assert_eq!(asset_byte_range("bytes=0-1,4-5", 10), None);
        assert_eq!(asset_byte_range("items=0-1", 10), None);
    }

    #[test]
    fn checks_content_length_like_number() {
        assert!(content_length_matches("6", 6.0));
        assert!(content_length_matches(" 6.0 ", 6.0));
        assert!(!content_length_matches("", 6.0));
        assert!(!content_length_matches("6.5", 6.0));
        assert!(!content_length_matches("abc", 6.0));
    }
}
