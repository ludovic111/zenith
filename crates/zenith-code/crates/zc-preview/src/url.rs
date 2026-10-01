//! `packages/shared/src/preview.ts`: tab ids, loopback hosts and URL normalization shared by
//! the preview server, the desktop and the web renderer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

static NEXT_TAB_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn base36(mut value: u64) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(digits[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `newPreviewTabId`: `tab_` and a process-wide sequence in base 36.
pub fn new_preview_tab_id() -> String {
    let sequence = NEXT_TAB_SEQUENCE.fetch_add(1, Ordering::SeqCst) + 1;
    format!("tab_{}", base36(sequence))
}

const LOOPBACK_HOSTS: [&str; 4] = ["localhost", "127.0.0.1", "0.0.0.0", "::1"];

/// `LSOF_LOCAL_HOST_TOKENS`: the host part of a listening socket that counts as local.
pub const LSOF_LOCAL_HOST_TOKENS: [&str; 7] = ["localhost", "127.0.0.1", "0.0.0.0", "::1", "*", "[::]", "[::1]"];

/// `isLoopbackHost` (a URL `hostname`; IPv6 keeps its brackets).
pub fn is_loopback_host(host: &str) -> bool {
    LOOPBACK_HOSTS.contains(&host) || host == "[::1]"
}

/// Why a URL was refused (`PreviewUrlNormalizationError.reason`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlNormalizationError {
    /// UTF-16 length of the raw input.
    pub input_length: usize,
    /// `empty`, `parse` or `unsupported-protocol`.
    pub reason: &'static str,
    pub protocol: Option<String>,
    /// The parser's own message, kept as the cause and never put in a message.
    pub cause: Option<String>,
}

impl UrlNormalizationError {
    /// `PreviewUrlNormalizationError.message`.
    pub fn message(&self) -> String {
        let protocol = self.protocol.as_ref().map(|protocol| format!(": {protocol}")).unwrap_or_default();
        format!("Invalid preview URL ({}{protocol}; input length {}).", self.reason, self.input_length)
    }

    /// The encoded `PreviewUrlNormalizationError`, as the `cause` of `PreviewInvalidUrlError`.
    pub fn encoded(&self) -> Value {
        let mut fields = Map::new();
        fields.insert("_tag".into(), json!("PreviewUrlNormalizationError"));
        fields.insert("inputLength".into(), json!(self.input_length));
        fields.insert("reason".into(), json!(self.reason));
        if let Some(protocol) = &self.protocol {
            fields.insert("protocol".into(), json!(protocol));
        }
        if let Some(cause) = &self.cause {
            fields.insert("cause".into(), json!({"name": "TypeError", "message": cause}));
        }
        Value::Object(fields)
    }
}

/// The scheme of a raw URL, lower-cased with its colon (`previewUrlProtocol`).
fn url_protocol(raw: &str) -> Option<String> {
    static SCHEME: OnceLock<Regex> = OnceLock::new();
    let scheme = SCHEME.get_or_init(|| Regex::new(r"^([A-Za-z][A-Za-z\d+.\-]*):").expect("static regex"));
    scheme
        .captures(raw)
        .and_then(|captures| captures.get(1))
        .map(|scheme| format!("{}:", scheme.as_str().to_lowercase()))
}

/// `String.prototype.trim`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `normalizePreviewUrl`: a free-form URL as a full `http(s)://` URL (`URL.href`). Bare
/// loopback hosts get `http://`, other bare hosts `https://`.
pub fn normalize_preview_url(raw: &str) -> Result<String, UrlNormalizationError> {
    static LOOPBACK_PREFIX: OnceLock<Regex> = OnceLock::new();
    let input_length = raw.encode_utf16().count();
    let trimmed = js_trim(raw);
    if trimmed.is_empty() {
        return Err(UrlNormalizationError {
            input_length,
            reason: "empty",
            protocol: None,
            cause: None,
        });
    }
    let loopback = LOOPBACK_PREFIX.get_or_init(|| Regex::new(r"(?i)^(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\])(?::|/|$)").expect("static regex"));
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("{}://{trimmed}", if loopback.is_match(trimmed) { "http" } else { "https" })
    };
    let parsed = url::Url::parse(&candidate).map_err(|error| UrlNormalizationError {
        input_length,
        reason: "parse",
        protocol: url_protocol(&candidate),
        cause: Some(error.to_string()),
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(UrlNormalizationError {
            input_length,
            reason: "unsupported-protocol",
            protocol: Some(format!("{}:", parsed.scheme())),
            cause: None,
        });
    }
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_the_shared_helper() {
        assert_eq!(normalize_preview_url("localhost:5173").unwrap(), "http://localhost:5173/");
        assert_eq!(normalize_preview_url("example.com").unwrap(), "https://example.com/");
        assert_eq!(normalize_preview_url("  http://localhost:5173/about ").unwrap(), "http://localhost:5173/about");
        assert_eq!(normalize_preview_url("[::1]:3000").unwrap(), "http://[::1]:3000/");
        let empty = normalize_preview_url("   ").unwrap_err();
        assert_eq!((empty.reason, empty.input_length), ("empty", 3));
        let raw = "https://user:password@example.com:bad/path?access_token=secret#fragment";
        let parse = normalize_preview_url(raw).unwrap_err();
        assert_eq!((parse.reason, parse.protocol.as_deref()), ("parse", Some("https:")));
        assert!(!parse.message().contains("secret") && !parse.message().contains("password"));
        let ftp = normalize_preview_url("ftp://example.com").unwrap_err();
        assert_eq!((ftp.reason, ftp.protocol.as_deref()), ("unsupported-protocol", Some("ftp:")));
    }

    #[test]
    fn tab_ids_count_in_base_36() {
        let first = new_preview_tab_id();
        assert!(first.starts_with("tab_"));
        assert_ne!(first, new_preview_tab_id());
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    #[test]
    fn knows_loopback_hosts() {
        for host in ["localhost", "127.0.0.1", "0.0.0.0", "::1", "[::1]"] {
            assert!(is_loopback_host(host));
        }
        assert!(!is_loopback_host("example.com"));
    }
}
