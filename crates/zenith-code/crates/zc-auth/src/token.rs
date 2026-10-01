//! The signed token format (`auth/utils.ts`, `auth/SessionStore.ts`):
//!
//! ```text
//! token = base64url(JSON(claims)) "." base64url(HMAC-SHA256(key, <that first part>))
//! session claims   {"v":1,"kind":"session","sid","sub","scopes","method","jkt"?,"iat","exp"}
//! websocket claims {"v":1,"kind":"websocket","sid","iat","exp"}
//! ```
//!
//! `iat` and `exp` are epoch milliseconds. Keys are written in that order, compact, exactly as
//! `JSON.stringify` writes them, so a token minted here is byte-for-byte what the TS server
//! would mint with the same inputs.
//!
//! Decoding follows the TS quirks that matter for compatibility: `token.split(".")` keeps the
//! first two parts (later ones are ignored), and the signature comparison decodes both sides
//! the way Node's `Buffer.from(_, "base64url")` does (unknown characters are skipped, `=` ends
//! the data).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zc_contracts::{AuthEnvironmentScope as Scope, ServerAuthSessionMethod as SessionMethod};

use crate::scopes::parse_scope;

/// `Encoding.encodeBase64Url`: URL-safe alphabet, no padding.
pub fn base64url_encode(bytes: impl AsRef<[u8]>) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn base64_value(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// Node's `Buffer.from(input, "base64url")`: both alphabets, characters outside them skipped,
/// decoding stops at the first `=`, a dangling single sextet is dropped. Never fails.
pub fn node_base64url_decode(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in input.chars() {
        if c == '=' {
            break;
        }
        let Some(value) = u8::try_from(c).ok().and_then(base64_value) else {
            continue;
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Effect's `Encoding.decodeBase64Url`: CR/LF stripped, URL-safe alphabet with optional
/// padding, `None` when invalid.
pub fn effect_base64url_decode(input: &str) -> Option<Vec<u8>> {
    let stripped: String = input.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if stripped.len() % 4 == 1 {
        return None;
    }
    // /^[-_A-Z0-9]*?={0,2}$/i
    let body = stripped.trim_end_matches('=');
    if stripped.len() - body.len() > 2 || !body.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
        return None;
    }
    // Effect pads to a multiple of 4, then its base64 decoder only accepts `=` at the end.
    let mut sanitized = stripped.clone();
    match stripped.len() % 4 {
        2 => sanitized.push_str("=="),
        3 => sanitized.push('='),
        _ => {}
    }
    let len = sanitized.len();
    if let Some(index) = sanitized.find('=') {
        if index < len - 2 || (index == len - 2 && !sanitized.ends_with('=')) {
            return None;
        }
    }
    base64::engine::general_purpose::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    )
    .decode(body)
    .ok()
}

/// `signPayload`: base64url(HMAC-SHA256(secret, payload)).
pub fn sign_payload(payload: &str, secret: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(payload.as_bytes());
    base64url_encode(mac.finalize().into_bytes())
}

/// `timingSafeEqualBase64Url`: compares the decoded bytes in constant time.
pub fn timing_safe_equal_base64url(left: &str, right: &str) -> bool {
    let left = node_base64url_decode(left);
    let right = node_base64url_decode(right);
    left.len() == right.len() && bool::from(left.ct_eq(&right))
}

/// base64url(SHA-256(input)).
pub fn sha256_base64url(input: &[u8]) -> String {
    base64url_encode(Sha256::digest(input))
}

/// The first two `.`-separated parts of a token (`token.split(".")`), `None` when either is
/// empty or missing.
pub fn split_token(token: &str) -> Option<(&str, &str)> {
    let mut parts = token.split('.');
    let payload = parts.next().filter(|p| !p.is_empty())?;
    let signature = parts.next().filter(|p| !p.is_empty())?;
    Some((payload, signature))
}

/// Signs encoded claims: `<base64url(json)>.<signature>`.
pub fn sign_claims(claims_json: &str, secret: &[u8]) -> String {
    let encoded = base64url_encode(claims_json.as_bytes());
    let signature = sign_payload(&encoded, secret);
    format!("{encoded}.{signature}")
}

/// `SessionClaims`.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionClaims {
    pub sid: String,
    pub sub: String,
    pub scopes: Vec<Scope>,
    pub method: SessionMethod,
    pub jkt: Option<String>,
    /// `Schema.Number`; the server always writes whole milliseconds.
    pub iat: f64,
    pub exp: f64,
}

/// `WebSocketClaims`.
#[derive(Clone, Debug, PartialEq)]
pub struct WebSocketClaims {
    pub sid: String,
    pub iat: f64,
    pub exp: f64,
}

fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn method_from_str(value: &str) -> Option<SessionMethod> {
    SessionMethod::ALL.iter().copied().find(|m| m.as_str() == value)
}

impl SessionClaims {
    /// `JSON.stringify` of the encoded claims.
    pub fn to_json(&self) -> String {
        let mut map = Map::new();
        map.insert("v".into(), 1.into());
        map.insert("kind".into(), "session".into());
        map.insert("sid".into(), self.sid.clone().into());
        map.insert("sub".into(), self.sub.clone().into());
        map.insert("scopes".into(), self.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().into());
        map.insert("method".into(), self.method.as_str().into());
        if let Some(jkt) = &self.jkt {
            map.insert("jkt".into(), jkt.clone().into());
        }
        map.insert("iat".into(), number(self.iat));
        map.insert("exp".into(), number(self.exp));
        Value::Object(map).to_string()
    }

    /// `Schema.decodeUnknown(Schema.fromJsonString(SessionClaims))`.
    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let object = value.as_object().ok_or("Expected an object")?;
        expect_literal_number(object, "v", 1.0)?;
        expect_literal_string(object, "kind", "session")?;
        let sid = trimmed_non_empty(object, "sid")?;
        let sub = string_field(object, "sub")?;
        let scopes = match object.get("scopes") {
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| item.as_str().and_then(parse_scope))
                .collect::<Option<Vec<_>>>()
                .ok_or("scopes: Expected an AuthEnvironmentScope")?,
            _ => return Err("scopes: Expected an array".into()),
        };
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .and_then(method_from_str)
            .ok_or("method: Expected a session method")?;
        let jkt = match object.get("jkt") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err("jkt: Expected a string".into()),
        };
        let iat = number_field(object, "iat")?;
        let exp = number_field(object, "exp")?;
        Ok(Self {
            sid,
            sub,
            scopes,
            method,
            jkt,
            iat,
            exp,
        })
    }
}

impl WebSocketClaims {
    pub fn to_json(&self) -> String {
        let mut map = Map::new();
        map.insert("v".into(), 1.into());
        map.insert("kind".into(), "websocket".into());
        map.insert("sid".into(), self.sid.clone().into());
        map.insert("iat".into(), number(self.iat));
        map.insert("exp".into(), number(self.exp));
        Value::Object(map).to_string()
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let object = value.as_object().ok_or("Expected an object")?;
        expect_literal_number(object, "v", 1.0)?;
        expect_literal_string(object, "kind", "websocket")?;
        Ok(Self {
            sid: trimmed_non_empty(object, "sid")?,
            iat: number_field(object, "iat")?,
            exp: number_field(object, "exp")?,
        })
    }
}

fn expect_literal_number(object: &Map<String, Value>, key: &str, expected: f64) -> Result<(), String> {
    match object.get(key).and_then(Value::as_f64) {
        Some(value) if value == expected => Ok(()),
        _ => Err(format!("{key}: Expected {expected}")),
    }
}

fn expect_literal_string(object: &Map<String, Value>, key: &str, expected: &str) -> Result<(), String> {
    match object.get(key).and_then(Value::as_str) {
        Some(value) if value == expected => Ok(()),
        _ => Err(format!("{key}: Expected \"{expected}\"")),
    }
}

fn string_field(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("{key}: Expected a string"))
}

/// `TrimmedNonEmptyString`: trimmed on decode, then non-empty.
fn trimmed_non_empty(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    let value = string_field(object, key)?;
    let trimmed = js_trim(&value);
    if trimmed.is_empty() {
        Err(format!("{key}: Expected a non-empty string"))
    } else {
        Ok(trimmed.to_owned())
    }
}

fn number_field(object: &Map<String, Value>, key: &str) -> Result<f64, String> {
    object.get(key).and_then(Value::as_f64).ok_or_else(|| format!("{key}: Expected a number"))
}

/// `String.prototype.trim`: strips JS whitespace (Unicode `White_Space` plus BOM).
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Decodes the payload part of a token into its JSON text (`base64UrlDecodeUtf8`).
pub fn decode_payload_text(encoded: &str) -> Option<String> {
    effect_base64url_decode(encoded).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// A whole-millisecond `exp`/`iat` claim as a `DateTime` (`DateTime.make(number)`): `None`
/// outside the `Date` range.
pub fn claim_millis(value: f64) -> Option<i64> {
    if value.is_finite() && value.abs() <= 8.64e15 {
        Some(value.trunc() as i64)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_decoding_is_lenient_like_buffer_from() {
        let hex = |s: &str| node_base64url_decode(s).iter().map(|b| format!("{b:02x}")).collect::<String>();
        // Expectations measured with Node 26's Buffer.from(_, "base64url").
        assert_eq!(hex("YQ"), "61");
        assert_eq!(hex("YQ=="), "61");
        assert_eq!(hex("YQ==YQ=="), "61");
        assert_eq!(hex("Y Q"), "61");
        assert_eq!(hex("Y*Q"), "61");
        assert_eq!(hex("YQ+/"), "610fbf");
        assert_eq!(hex("YQ-_"), "610fbf");
        assert_eq!(hex("YWJj!"), "616263");
        assert_eq!(hex("Y"), "");
        assert_eq!(hex("YWJjZA="), "61626364");
        assert_eq!(hex("Y=Q"), "");
        assert_eq!(hex("YQ=xYWJj"), "61");
        assert_eq!(hex("YWJ=jZA"), "6162");
        assert_eq!(hex("\u{e9}YQ"), "61");
        assert_eq!(hex("YW\nJj"), "616263");
    }

    #[test]
    fn effect_decoding_is_strict() {
        assert_eq!(effect_base64url_decode("aGVsbG8_"), Some(b"hello?".to_vec()));
        assert_eq!(effect_base64url_decode("YQ"), Some(b"a".to_vec()));
        assert_eq!(effect_base64url_decode("YQ=="), Some(b"a".to_vec()));
        assert_eq!(effect_base64url_decode("YQ="), Some(b"a".to_vec()));
        assert_eq!(effect_base64url_decode("YWJ="), Some(b"ab".to_vec()));
        assert_eq!(effect_base64url_decode("Y=="), None);
        assert_eq!(effect_base64url_decode("YWJjZ="), None);
        assert_eq!(effect_base64url_decode("Y"), None);
        assert_eq!(effect_base64url_decode("Y*Q="), None);
        assert_eq!(effect_base64url_decode("YQ+/"), None);
    }

    #[test]
    fn claims_encode_like_json_stringify() {
        let claims = SessionClaims {
            sid: "11111111-2222-4333-8444-555555555555".into(),
            sub: "browser".into(),
            scopes: vec![Scope::OrchestrationRead, Scope::AccessWrite],
            method: SessionMethod::BrowserSessionCookie,
            jkt: None,
            iat: 1_790_000_000_000.0,
            exp: 1_792_592_000_000.0,
        };
        let json = claims.to_json();
        assert_eq!(
            json,
            r#"{"v":1,"kind":"session","sid":"11111111-2222-4333-8444-555555555555","sub":"browser","scopes":["orchestration:read","access:write"],"method":"browser-session-cookie","iat":1790000000000,"exp":1792592000000}"#
        );
        assert_eq!(SessionClaims::from_json(&json).unwrap(), claims);
        let ws = WebSocketClaims {
            sid: "s".into(),
            iat: 1.0,
            exp: 2.0,
        };
        assert_eq!(ws.to_json(), r#"{"v":1,"kind":"websocket","sid":"s","iat":1,"exp":2}"#);
        assert_eq!(WebSocketClaims::from_json(&ws.to_json()).unwrap(), ws);
        // A websocket ticket is not a session token and the reverse.
        assert!(SessionClaims::from_json(&ws.to_json()).is_err());
        assert!(WebSocketClaims::from_json(&json).is_err());
        assert!(SessionClaims::from_json(r#"{"v":1,"kind":"session","sid":" ","sub":"","scopes":[],"method":"bearer-access-token","iat":1,"exp":2}"#).is_err());
        assert!(SessionClaims::from_json(
            r#"{"v":1,"kind":"session","sid":"a","sub":"","scopes":[],"method":"bearer-access-token","jkt":null,"iat":1,"exp":2}"#
        )
        .is_err());
    }

    #[test]
    fn signing_matches_node_crypto() {
        // node -e 'console.log(require("crypto").createHmac("sha256", Buffer.from("k")).update("p").digest("base64url"))'
        assert_eq!(sign_payload("p", b"k"), "0X7k9pKRiAJX4-4oz0LR4Topuj_rYTLMFRdVTPcnEpk");
        assert!(timing_safe_equal_base64url("YWJj", "YWJj!"));
        assert!(!timing_safe_equal_base64url("YWJj", "YWJk"));
        assert_eq!(split_token("a.b.c"), Some(("a", "b")));
        assert_eq!(split_token("a."), None);
        assert_eq!(split_token("abc"), None);
    }
}
