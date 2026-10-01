//! Session cookie names (`auth/utils.ts`), the `Cookie` header parser and the `Set-Cookie`
//! serializer of Effect's `Cookies` module, as the TS server uses them.

use http::HeaderMap;
use sha2::{Digest, Sha256};

const SESSION_COOKIE_NAME: &str = "t3_session";

/// `--mode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerMode {
    Web,
    Desktop,
}

/// What the cookie name is derived from (`resolveSessionCookieName`'s input).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CookieNameInput {
    pub mode: ServerMode,
    pub port: u16,
    pub host: Option<String>,
    /// The state directory, as the server resolved it (`config.stateDir`).
    pub instance_key: String,
    pub environment_id: String,
    /// A dev URL is configured.
    pub development: bool,
}

/// `isRemoteReachableHost`.
pub fn is_remote_reachable_host(host: Option<&str>) -> bool {
    match host {
        Some("0.0.0.0" | "::" | "[::]") => true,
        None | Some("") => false,
        Some(host) => !(host == "localhost" || host == "127.0.0.1" || host == "::1" || host == "[::1]" || host.starts_with("127.")),
    }
}

fn sha256_hex_prefix(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// `resolveSessionCookieName`:
/// - desktop: `t3_session_<port>`;
/// - remote-reachable web (not dev): `t3_session_<sha256(environmentId)[..12]>`;
/// - otherwise (zenith: loopback web): `t3_session_<port>_<sha256(stateDir)[..12]>`.
pub fn resolve_session_cookie_name(input: &CookieNameInput) -> String {
    if input.mode == ServerMode::Desktop {
        return format!("{SESSION_COOKIE_NAME}_{}", input.port);
    }
    let remote = !input.development && is_remote_reachable_host(input.host.as_deref());
    let hash = sha256_hex_prefix(if remote { &input.environment_id } else { &input.instance_key });
    if remote {
        format!("{SESSION_COOKIE_NAME}_{hash}")
    } else {
        format!("{SESSION_COOKIE_NAME}_{}_{hash}", input.port)
    }
}

/// `resolveLegacySessionCookieName`: `t3_session` for remote-reachable web servers only.
pub fn resolve_legacy_session_cookie_name(input: &CookieNameInput) -> Option<String> {
    (input.mode == ServerMode::Web && !input.development && is_remote_reachable_host(input.host.as_deref())).then(|| SESSION_COOKIE_NAME.to_owned())
}

/// `decodeURIComponent`, or `None` where it throws (bad escape, invalid UTF-8).
fn decode_uri_component(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            let text = std::str::from_utf8(hex).ok()?;
            out.push(u8::from_str_radix(text, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Effect's `Cookies.parseHeader`: `;`-separated `name=value` pairs, the first occurrence of a
/// name wins, values are trimmed, surrounding quotes dropped, `%` escapes decoded when valid.
pub fn parse_cookie_header(header: &str) -> Vec<(String, String)> {
    // JS string indices over UTF-16; Node decodes header bytes as Latin-1, so every char of
    // the header is one code unit here.
    let chars: Vec<char> = header.chars().collect();
    let len = chars.len();
    let find = |from: usize, needle: char| chars[from.min(len)..].iter().position(|c| *c == needle).map(|p| p + from);
    let slice = |from: usize, to: usize| -> String {
        if from >= to {
            String::new()
        } else {
            chars[from..to.min(len)].iter().collect()
        }
    };
    let mut result: Vec<(String, String)> = Vec::new();
    let mut pos = 0usize;
    let mut terminator = 0usize;
    loop {
        if terminator == len {
            break;
        }
        terminator = find(pos, ';').unwrap_or(len);
        let Some(mut eq) = find(pos, '=') else { break };
        if eq > terminator {
            pos = terminator + 1;
            continue;
        }
        let key = slice(pos, eq).trim().to_owned();
        eq += 1;
        if !result.iter().any(|(k, _)| *k == key) {
            let raw = if chars.get(eq) == Some(&'"') {
                slice(eq + 1, terminator.saturating_sub(1))
            } else {
                slice(eq, terminator)
            };
            let value = raw.trim().to_owned();
            let value = if value.contains('%') {
                decode_uri_component(&value).unwrap_or(value)
            } else {
                value
            };
            result.push((key, value));
        }
        pos = terminator + 1;
    }
    result
}

/// The request's cookies (every `Cookie` header, joined like Node joins them).
pub fn request_cookies(headers: &HeaderMap) -> Vec<(String, String)> {
    let joined = headers
        .get_all(http::header::COOKIE)
        .iter()
        .map(|value| value.as_bytes().iter().map(|b| char::from(*b)).collect::<String>())
        .collect::<Vec<_>>()
        .join("; ");
    if joined.is_empty() {
        return Vec::new();
    }
    parse_cookie_header(&joined)
}

/// The value of cookie `name`, if present (possibly empty).
pub fn cookie_value<'a>(cookies: &'a [(String, String)], name: &str) -> Option<&'a str> {
    cookies.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}

/// `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `Date.prototype.toUTCString` (`Thu, 01 Oct 2026 12:00:00 GMT`).
pub fn to_utc_string(millis: i64) -> String {
    match jiff::Timestamp::from_millisecond(millis) {
        Ok(ts) => ts.to_zoned(jiff::tz::TimeZone::UTC).strftime("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        Err(_) => "Invalid Date".to_owned(),
    }
}

/// The session `Set-Cookie` value: `HttpOnly; Path=/; SameSite=Lax; Expires=<exp>`, written in
/// Effect's attribute order (`name=value; Path=/; Expires=…; HttpOnly; SameSite=Lax`).
pub fn session_set_cookie(name: &str, token: &str, expires_millis: i64) -> String {
    format!(
        "{name}={}; Path=/; Expires={}; HttpOnly; SameSite=Lax",
        encode_uri_component(token),
        to_utc_string(expires_millis)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(mode: ServerMode, port: u16, host: &str, key: &str, env: &str, dev: bool) -> CookieNameInput {
        CookieNameInput {
            mode,
            port,
            host: Some(host.to_owned()),
            instance_key: key.into(),
            environment_id: env.into(),
            development: dev,
        }
    }

    // utils.test.ts "session cookie isolation"
    #[test]
    fn isolates_loopback_web_servers_by_port_and_state() {
        let first = resolve_session_cookie_name(&input(ServerMode::Web, 5775, "127.0.0.1", "/tmp/t3-agent-one", "environment-one", true));
        let second = resolve_session_cookie_name(&input(ServerMode::Web, 5775, "127.0.0.1", "/tmp/t3-agent-two", "environment-two", true));
        let re = regex::Regex::new(r"^t3_session_5775_[a-f0-9]{12}$").unwrap();
        assert!(re.is_match(&first) && re.is_match(&second));
        assert_ne!(first, second);
        // sha256("/tmp/t3-agent-one")[..12], computed with node:crypto.
        assert_eq!(first, format!("t3_session_5775_{}", sha256_hex_prefix("/tmp/t3-agent-one")));
    }

    #[test]
    fn isolates_remote_web_servers_by_environment() {
        let first = resolve_session_cookie_name(&input(ServerMode::Web, 3773, "192.168.1.50", "/srv/t3-one", "environment-one", false));
        let second = resolve_session_cookie_name(&input(ServerMode::Web, 5775, "192.168.1.50", "/srv/t3-two", "environment-two", false));
        let re = regex::Regex::new(r"^t3_session_[a-f0-9]{12}$").unwrap();
        assert!(re.is_match(&first) && re.is_match(&second));
        assert_ne!(first, second);
        let a = resolve_session_cookie_name(&input(ServerMode::Web, 8080, "0.0.0.0", "/srv/t3", "environment-one", false));
        let b = resolve_session_cookie_name(&input(ServerMode::Web, 9090, "app.example.com", "/srv/t3", "environment-one", false));
        assert_eq!(a, b);
        assert_eq!(
            resolve_legacy_session_cookie_name(&input(ServerMode::Web, 1, "0.0.0.0", "", "", false)).as_deref(),
            Some("t3_session")
        );
        assert_eq!(resolve_legacy_session_cookie_name(&input(ServerMode::Web, 1, "127.0.0.1", "", "", false)), None);
    }

    #[test]
    fn desktop_and_dev_wildcard() {
        assert_eq!(
            resolve_session_cookie_name(&input(ServerMode::Desktop, 3773, "127.0.0.1", "/tmp/desktop", "environment-one", true)),
            "t3_session_3773"
        );
        let dev = resolve_session_cookie_name(&input(ServerMode::Web, 5775, "0.0.0.0", "/tmp/t3-wildcard-dev", "environment-one", true));
        assert!(regex::Regex::new(r"^t3_session_5775_[a-f0-9]{12}$").unwrap().is_match(&dev));
    }

    #[test]
    fn classifies_hosts() {
        assert!(!is_remote_reachable_host(None));
        assert!(!is_remote_reachable_host(Some("localhost")));
        assert!(!is_remote_reachable_host(Some("127.12.0.1")));
        assert!(!is_remote_reachable_host(Some("[::1]")));
        assert!(is_remote_reachable_host(Some("0.0.0.0")));
        assert!(is_remote_reachable_host(Some("192.168.1.50")));
    }

    #[test]
    fn parses_cookie_headers_like_effect() {
        let parsed = parse_cookie_header("a=1; b = two ; a=3; c=\"quoted\"; d=%41%42; e=%E0%A4%A; noeq; f=");
        assert_eq!(cookie_value(&parsed, "a"), Some("1"));
        assert_eq!(cookie_value(&parsed, "b"), Some("two"));
        assert_eq!(cookie_value(&parsed, "c"), Some("quoted"));
        assert_eq!(cookie_value(&parsed, "d"), Some("AB"));
        assert_eq!(cookie_value(&parsed, "e"), Some("%E0%A4%A"));
        assert_eq!(cookie_value(&parsed, "f"), Some(""));
        assert_eq!(cookie_value(&parsed, "noeq"), None);
    }

    #[test]
    fn serializes_the_session_cookie() {
        assert_eq!(
            session_set_cookie("t3_session_1_abc", "a.b-c_d", 1_793_461_837_933),
            "t3_session_1_abc=a.b-c_d; Path=/; Expires=Sat, 31 Oct 2026 15:50:37 GMT; HttpOnly; SameSite=Lax"
        );
    }
}
