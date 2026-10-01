//! DPoP (RFC 9449) proofs, as `packages/shared/src/dpop.ts` verifies them and
//! `auth/dpop.ts` records them; plus the replay-marker sweep of `auth/replayMarkers.ts`.
//!
//! A proof is a compact ES256 JWT: header `{"typ":"dpop+jwt","alg":"ES256","jwk":{P-256 public
//! key}}`, payload `{htm, htu, jti, iat, ath?}`. Checks, in order: header/payload decode,
//! key thumbprint, method, URL (`htu` = request URL without query and fragment, rebuilt from
//! `Host` and `x-forwarded-proto`), access-token hash, signature (raw `r||s`, high-S accepted
//! like noble's `verify` with explicit options), then `iat` within +5 s / −300 s.
//!
//! Each accepted proof leaves a marker `secrets/dpop-proof-<b64url(sha256(jkt:jti))>.bin`
//! created with `O_EXCL`: a second use of the same proof fails with `replay`.

use std::path::Path;
use std::time::{Duration, SystemTime};

use p256::ecdsa::signature::hazmat::PrehashVerifier;
use p256::ecdsa::{Signature, VerifyingKey};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use zc_contracts::DpopFailureReason;
use zc_core::ServerSecretStore;

use crate::error::{internal, ServerAuthError};
use crate::token::{effect_base64url_decode, sha256_base64url};

/// Secret-store prefix of DPoP replay markers.
pub const DPOP_REPLAY_MARKER_PREFIX: &str = "dpop-proof-";
/// Prefixes of every replay marker the sweep removes (DPoP and the cloud proofs).
pub const REPLAY_MARKER_PREFIXES: &[&str] = &[
    DPOP_REPLAY_MARKER_PREFIX,
    "cloud-mint-nonce-",
    "cloud-mint-jti-",
    "cloud-health-nonce-",
    "cloud-health-jti-",
];
/// A marker older than a day is useless: the time check alone rejects its proof.
pub const REPLAY_MARKER_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;
const DEFAULT_MAX_AGE_SECONDS: i64 = 300;

/// `DpopVerificationFailureCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DpopFailureCode {
    MissingProof,
    MalformedProof,
    KeyMismatch,
    MethodMismatch,
    UrlMismatch,
    AccessTokenHashMismatch,
    TimeWindow,
    InvalidSignature,
    InvalidProof,
}

impl DpopFailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingProof => "missing_proof",
            Self::MalformedProof => "malformed_proof",
            Self::KeyMismatch => "key_mismatch",
            Self::MethodMismatch => "method_mismatch",
            Self::UrlMismatch => "url_mismatch",
            Self::AccessTokenHashMismatch => "access_token_hash_mismatch",
            Self::TimeWindow => "time_window",
            Self::InvalidSignature => "invalid_signature",
            Self::InvalidProof => "invalid_proof",
        }
    }

    /// `mapDpopFailureReason`: the safe category a client sees.
    pub fn reason(self) -> DpopFailureReason {
        match self {
            Self::TimeWindow => DpopFailureReason::TimeWindow,
            Self::KeyMismatch => DpopFailureReason::KeyMismatch,
            Self::MethodMismatch | Self::UrlMismatch => DpopFailureReason::RequestMismatch,
            Self::AccessTokenHashMismatch => DpopFailureReason::TokenMismatch,
            Self::MissingProof | Self::MalformedProof | Self::InvalidSignature | Self::InvalidProof => DpopFailureReason::InvalidProof,
        }
    }
}

/// A failed verification: the code and the TS diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DpopFailure {
    pub code: DpopFailureCode,
    pub reason: &'static str,
}

/// A verified proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedDpopProof {
    pub thumbprint: String,
    pub jti: String,
    pub iat: i64,
}

/// What a proof is checked against.
#[derive(Clone, Debug, Default)]
pub struct DpopCheck<'a> {
    pub proof: Option<&'a str>,
    pub method: &'a str,
    pub url: &'a str,
    pub now_epoch_seconds: i64,
    pub expected_thumbprint: Option<&'a str>,
    pub expected_access_token: Option<&'a str>,
    pub max_age_seconds: Option<i64>,
}

fn fail(code: DpopFailureCode, reason: &'static str) -> DpopFailure {
    DpopFailure { code, reason }
}

/// The public JWK of a proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DpopPublicJwk {
    pub kty: String,
    pub crv: String,
    pub x: String,
    pub y: String,
}

/// `stableStringify({crv, kty, x, y})`: sorted keys, `JSON.stringify` values.
fn thumbprint_input(jwk: &DpopPublicJwk) -> String {
    let s = |v: &str| Value::String(v.to_owned()).to_string();
    format!("{{\"crv\":{},\"kty\":{},\"x\":{},\"y\":{}}}", s(&jwk.crv), s(&jwk.kty), s(&jwk.x), s(&jwk.y))
}

/// `computeDpopJwkThumbprint` (RFC 7638).
pub fn compute_jwk_thumbprint(jwk: &DpopPublicJwk) -> String {
    sha256_base64url(thumbprint_input(jwk).as_bytes())
}

/// `computeDpopAccessTokenHash`: the `ath` claim.
pub fn compute_access_token_hash(access_token: &str) -> String {
    sha256_base64url(access_token.as_bytes())
}

/// `normalizeDpopHtu`: the URL without query and fragment, WHATWG-serialized.
pub fn normalize_htu(url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(url).ok()?;
    parsed.set_fragment(None);
    parsed.set_query(None);
    Some(parsed.to_string())
}

fn non_empty_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn decode_header(text: &str) -> Option<DpopPublicJwk> {
    let value: Value = serde_json::from_str(text).ok()?;
    let header = value.as_object()?;
    if header.get("typ")?.as_str()? != "dpop+jwt" || header.get("alg")?.as_str()? != "ES256" {
        return None;
    }
    let jwk = header.get("jwk")?.as_object()?;
    if jwk.get("kty")?.as_str()? != "EC" || jwk.get("crv")?.as_str()? != "P-256" {
        return None;
    }
    // `d: Schema.optionalKey(Schema.Never)`: a private key must never be sent.
    if jwk.contains_key("d") {
        return None;
    }
    Some(DpopPublicJwk {
        kty: "EC".into(),
        crv: "P-256".into(),
        x: non_empty_string(jwk, "x")?,
        y: non_empty_string(jwk, "y")?,
    })
}

struct DpopPayload {
    htm: String,
    htu: String,
    jti: String,
    iat: i64,
    ath: Option<String>,
}

fn decode_payload(text: &str) -> Option<DpopPayload> {
    let value: Value = serde_json::from_str(text).ok()?;
    let payload = value.as_object()?;
    let iat = payload.get("iat")?.as_f64()?;
    if !iat.is_finite() || iat.fract() != 0.0 {
        return None;
    }
    let ath = match payload.get("ath") {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    Some(DpopPayload {
        htm: non_empty_string(payload, "htm")?,
        htu: non_empty_string(payload, "htu")?,
        jti: non_empty_string(payload, "jti")?,
        iat: iat as i64,
        ath,
    })
}

fn decode_text(part: &str) -> Result<String, ()> {
    effect_base64url_decode(part)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .ok_or(())
}

/// `verifyDpopProof`.
pub fn verify_dpop_proof(check: &DpopCheck<'_>) -> Result<VerifiedDpopProof, DpopFailure> {
    let Some(proof) = check.proof.filter(|p| !crate::token::js_trim(p).is_empty()) else {
        return Err(fail(DpopFailureCode::MissingProof, "Missing DPoP proof."));
    };
    let parts: Vec<&str> = proof.split('.').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
        return Err(fail(DpopFailureCode::MalformedProof, "Invalid DPoP compact JWT."));
    }
    let invalid = || fail(DpopFailureCode::InvalidProof, "Invalid DPoP proof.");
    // The base64 decodes throw in TS, which its catch-all reports as `invalid_proof`.
    let header = decode_text(parts[0]).map_err(|()| invalid())?;
    let payload = decode_text(parts[1]).map_err(|()| invalid())?;
    let jwk = decode_header(&header).ok_or_else(|| fail(DpopFailureCode::MalformedProof, "Invalid DPoP JWT header."))?;
    let payload = decode_payload(&payload).ok_or_else(|| fail(DpopFailureCode::MalformedProof, "Invalid DPoP JWT payload."))?;

    let thumbprint = compute_jwk_thumbprint(&jwk);
    if let Some(expected) = check.expected_thumbprint.filter(|t| !t.is_empty()) {
        if thumbprint != expected {
            return Err(fail(DpopFailureCode::KeyMismatch, "DPoP key thumbprint mismatch."));
        }
    }
    if payload.htm.to_uppercase() != check.method.to_uppercase() {
        return Err(fail(DpopFailureCode::MethodMismatch, "DPoP method mismatch."));
    }
    match normalize_htu(check.url) {
        Some(htu) if htu == payload.htu => {}
        _ => return Err(fail(DpopFailureCode::UrlMismatch, "DPoP URL mismatch.")),
    }
    if let Some(token) = check.expected_access_token.filter(|t| !t.is_empty()) {
        if payload.ath.as_deref() != Some(compute_access_token_hash(token).as_str()) {
            return Err(fail(DpopFailureCode::AccessTokenHashMismatch, "DPoP access token hash mismatch."));
        }
    }

    let signature = effect_base64url_decode(parts[2]).ok_or_else(invalid)?;
    let x = effect_base64url_decode(&jwk.x).ok_or_else(invalid)?;
    let y = effect_base64url_decode(&jwk.y).ok_or_else(invalid)?;
    if x.len() != 32 || y.len() != 32 {
        return Err(invalid());
    }
    let mut point = Vec::with_capacity(65);
    point.push(0x04);
    point.extend_from_slice(&x);
    point.extend_from_slice(&y);
    let digest = Sha256::digest(format!("{}.{}", parts[0], parts[1]).as_bytes());
    let verified = (|| {
        let key = VerifyingKey::from_sec1_bytes(&point).ok()?;
        let signature = Signature::from_slice(&signature).ok()?;
        key.verify_prehash(&digest, &signature).ok()
    })()
    .is_some();
    if !verified {
        return Err(fail(DpopFailureCode::InvalidSignature, "Invalid DPoP signature."));
    }

    let max_age = check.max_age_seconds.unwrap_or(DEFAULT_MAX_AGE_SECONDS);
    if payload.iat > check.now_epoch_seconds + 5 || check.now_epoch_seconds - payload.iat > max_age {
        return Err(fail(DpopFailureCode::TimeWindow, "DPoP proof is outside the allowed time window."));
    }
    Ok(VerifiedDpopProof {
        thumbprint,
        jti: payload.jti,
        iat: payload.iat,
    })
}

/// The request a proof belongs to (`HttpServerRequest.toURL`): `http://<Host><url>`, or
/// `https://` when `x-forwarded-proto: https`.
pub fn request_url(host: Option<&str>, forwarded_proto: Option<&str>, path_and_query: &str) -> Option<String> {
    let protocol = if forwarded_proto == Some("https") { "https" } else { "http" };
    let base = url::Url::parse(&format!("{protocol}://{}", host.unwrap_or("localhost"))).ok()?;
    base.join(path_and_query).ok().map(|u| u.to_string())
}

/// `verifyRequestDpopProof`: verifies the proof and records its replay marker; returns the key
/// thumbprint.
pub async fn verify_request_dpop_proof(
    secrets: &ServerSecretStore,
    proof: Option<&str>,
    method: &str,
    url: Option<&str>,
    now_millis: i64,
    expected_thumbprint: Option<&str>,
    expected_access_token: Option<&str>,
) -> Result<String, ServerAuthError> {
    let Some(url) = url else {
        return Err(ServerAuthError::InvalidCredential {
            diagnostic: Some("Invalid DPoP request URL.".into()),
            dpop_failure_reason: None,
            cause: None,
        });
    };
    let verified = verify_dpop_proof(&DpopCheck {
        proof,
        method,
        url,
        now_epoch_seconds: now_millis.div_euclid(1000),
        expected_thumbprint,
        expected_access_token,
        max_age_seconds: None,
    })
    .map_err(|failure| ServerAuthError::dpop(failure.reason, failure.code.reason()))?;
    let replay_key = sha256_base64url(format!("{}:{}", verified.thumbprint, verified.jti).as_bytes());
    let marker = format!(
        "thumbprint={}\njti={}\niat={}\nconsumedAt={}",
        verified.thumbprint,
        verified.jti,
        verified.iat,
        zc_core::iso_from_millis(now_millis)
    );
    match secrets.create(&format!("{DPOP_REPLAY_MARKER_PREFIX}{replay_key}"), marker.as_bytes()).await {
        Ok(()) => Ok(verified.thumbprint),
        Err(error) if error.is_already_exists() => Err(ServerAuthError::InvalidCredential {
            diagnostic: Some("DPoP proof replayed.".into()),
            dpop_failure_reason: Some(DpopFailureReason::Replay),
            cause: Some(error.to_string()),
        }),
        Err(error) => Err(internal::dpop_replay_state_record(error)),
    }
}

/// `pruneExpiredReplayMarkers`: removes `<prefix>*.bin` markers whose mtime is older than a
/// day. Other secrets and temp files are never touched. Returns how many were removed.
pub async fn prune_expired_replay_markers(secrets_dir: &Path, now_millis: i64) -> std::io::Result<usize> {
    let cutoff = now_millis - REPLAY_MARKER_MAX_AGE_MS;
    let mut entries = tokio::fs::read_dir(secrets_dir).await?;
    let mut removed = 0;
    let mut failures = 0;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.ends_with(".bin") || !REPLAY_MARKER_PREFIXES.iter().any(|p| name.starts_with(p)) {
            continue;
        }
        let path = entry.path();
        let modified = match tokio::fs::metadata(&path).await.and_then(|m| m.modified()) {
            Ok(modified) => modified,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                failures += 1;
                continue;
            }
        };
        let mtime = modified.duration_since(SystemTime::UNIX_EPOCH).unwrap_or(Duration::ZERO).as_millis() as i64;
        if mtime < cutoff {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => failures += 1,
            }
        }
    }
    if failures > 0 {
        tracing::warn!(failed = failures, "Failed to prune some replay markers");
    }
    Ok(removed)
}

/// Runs the sweep now and then every hour, forever (spawn it once the server is up).
pub async fn run_replay_marker_sweeper(secrets_dir: std::path::PathBuf) {
    let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
    loop {
        interval.tick().await;
        if let Err(cause) = prune_expired_replay_markers(&secrets_dir, zc_core::now_millis()).await {
            tracing::warn!(%cause, "Failed to prune expired replay markers");
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Minting proofs in tests (the client side of `packages/shared/src/dpop.ts`).
    use super::*;
    use crate::token::base64url_encode;
    use p256::ecdsa::signature::hazmat::PrehashSigner;
    use p256::ecdsa::SigningKey;

    pub struct TestKey {
        pub signing: SigningKey,
        pub jwk: DpopPublicJwk,
    }

    impl TestKey {
        pub fn new(seed: u8) -> Self {
            let signing = SigningKey::from_slice(&[seed.max(1); 32]).unwrap();
            let point = signing.verifying_key().to_encoded_point(false);
            Self {
                jwk: DpopPublicJwk {
                    kty: "EC".into(),
                    crv: "P-256".into(),
                    x: base64url_encode(point.x().unwrap()),
                    y: base64url_encode(point.y().unwrap()),
                },
                signing,
            }
        }

        pub fn thumbprint(&self) -> String {
            compute_jwk_thumbprint(&self.jwk)
        }

        pub fn proof(&self, htm: &str, htu: &str, jti: &str, iat: i64, access_token: Option<&str>) -> String {
            let header = serde_json::json!({
                "typ": "dpop+jwt",
                "alg": "ES256",
                "jwk": {"kty": "EC", "crv": "P-256", "x": self.jwk.x, "y": self.jwk.y},
            });
            let mut payload = serde_json::json!({"htm": htm, "htu": htu, "jti": jti, "iat": iat});
            if let Some(token) = access_token {
                payload["ath"] = compute_access_token_hash(token).into();
            }
            let signing_input = format!("{}.{}", base64url_encode(header.to_string()), base64url_encode(payload.to_string()));
            let digest = Sha256::digest(signing_input.as_bytes());
            let signature: Signature = self.signing.sign_prehash(&digest).unwrap();
            format!("{signing_input}.{}", base64url_encode(signature.to_bytes()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestKey;
    use super::*;

    const URL: &str = "http://127.0.0.1:3773/api/auth/session";

    fn check<'a>(proof: &'a str, now: i64) -> DpopCheck<'a> {
        DpopCheck {
            proof: Some(proof),
            method: "GET",
            url: URL,
            now_epoch_seconds: now,
            ..DpopCheck::default()
        }
    }

    #[test]
    fn verifies_a_valid_proof_and_its_bindings() {
        let key = TestKey::new(7);
        let proof = key.proof("get", "http://127.0.0.1:3773/api/auth/session", "j1", 1000, Some("tok"));
        let mut c = check(&proof, 1000);
        c.url = "http://127.0.0.1:3773/api/auth/session?x=1#frag";
        c.expected_thumbprint = Some("");
        let ok = verify_dpop_proof(&c).unwrap();
        assert_eq!(ok.thumbprint, key.thumbprint());
        assert_eq!(ok.jti, "j1");

        c.expected_thumbprint = Some("other");
        assert_eq!(verify_dpop_proof(&c).unwrap_err().code, DpopFailureCode::KeyMismatch);
        c.expected_thumbprint = None;
        c.expected_access_token = Some("other-token");
        assert_eq!(verify_dpop_proof(&c).unwrap_err().code, DpopFailureCode::AccessTokenHashMismatch);
        c.expected_access_token = Some("tok");
        c.method = "POST";
        assert_eq!(verify_dpop_proof(&c).unwrap_err().code, DpopFailureCode::MethodMismatch);
        c.method = "GET";
        c.url = "http://127.0.0.1:3773/api/auth/other";
        assert_eq!(verify_dpop_proof(&c).unwrap_err().code, DpopFailureCode::UrlMismatch);
    }

    #[test]
    fn enforces_the_time_window() {
        let key = TestKey::new(3);
        let proof = key.proof("GET", URL, "j", 1000, None);
        assert!(verify_dpop_proof(&check(&proof, 995)).is_ok());
        assert_eq!(verify_dpop_proof(&check(&proof, 994)).unwrap_err().code, DpopFailureCode::TimeWindow);
        assert!(verify_dpop_proof(&check(&proof, 1300)).is_ok());
        assert_eq!(verify_dpop_proof(&check(&proof, 1301)).unwrap_err().code, DpopFailureCode::TimeWindow);
    }

    #[test]
    fn rejects_malformed_and_forged_proofs() {
        let key = TestKey::new(5);
        let proof = key.proof("GET", URL, "j", 1000, None);
        assert_eq!(
            verify_dpop_proof(&DpopCheck { proof: None, ..check("", 0) }).unwrap_err().code,
            DpopFailureCode::MissingProof
        );
        assert_eq!(verify_dpop_proof(&check("  ", 1000)).unwrap_err().code, DpopFailureCode::MissingProof);
        assert_eq!(verify_dpop_proof(&check("a.b", 1000)).unwrap_err().code, DpopFailureCode::MalformedProof);
        assert_eq!(verify_dpop_proof(&check("a..c", 1000)).unwrap_err().code, DpopFailureCode::MalformedProof);
        assert_eq!(verify_dpop_proof(&check("*.e30.c", 1000)).unwrap_err().code, DpopFailureCode::InvalidProof);
        assert_eq!(verify_dpop_proof(&check("e30.e30.c", 1000)).unwrap_err().code, DpopFailureCode::MalformedProof);
        // Another key's signature over the same header and payload.
        let other = TestKey::new(9);
        let parts: Vec<&str> = proof.split('.').collect();
        let forged_sig = other.proof("GET", URL, "j", 1000, None);
        let forged = format!("{}.{}.{}", parts[0], parts[1], forged_sig.split('.').nth(2).unwrap());
        assert_eq!(verify_dpop_proof(&check(&forged, 1000)).unwrap_err().code, DpopFailureCode::InvalidSignature);
    }

    // dpop.test.ts "mapDpopFailureReason"
    #[test]
    fn maps_failures_to_safe_categories() {
        use DpopFailureCode::*;
        assert_eq!(TimeWindow.reason(), DpopFailureReason::TimeWindow);
        assert_eq!(KeyMismatch.reason(), DpopFailureReason::KeyMismatch);
        assert_eq!(MethodMismatch.reason(), DpopFailureReason::RequestMismatch);
        assert_eq!(UrlMismatch.reason(), DpopFailureReason::RequestMismatch);
        assert_eq!(AccessTokenHashMismatch.reason(), DpopFailureReason::TokenMismatch);
        for code in [MissingProof, MalformedProof, InvalidSignature, InvalidProof] {
            assert_eq!(code.reason(), DpopFailureReason::InvalidProof);
        }
    }

    #[test]
    fn rebuilds_the_request_url() {
        assert_eq!(
            request_url(Some("127.0.0.1:3773"), None, "/oauth/token?a=1").as_deref(),
            Some("http://127.0.0.1:3773/oauth/token?a=1")
        );
        assert_eq!(
            request_url(Some("Example.COM:443"), Some("https"), "/x").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(request_url(None, None, "/x").as_deref(), Some("http://localhost/x"));
    }

    // dpop.test.ts "mapDpopReplayStoreError" + the replay marker itself.
    #[tokio::test]
    async fn records_replay_markers_and_rejects_replays() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = ServerSecretStore::open(dir.path().join("secrets")).await.unwrap();
        let key = TestKey::new(11);
        let now = 1_790_000_000_000;
        let proof = key.proof("POST", "http://127.0.0.1:1/oauth/token", "jti-1", now / 1000, None);
        let url = Some("http://127.0.0.1:1/oauth/token");
        let thumbprint = verify_request_dpop_proof(&secrets, Some(&proof), "POST", url, now, None, None).await.unwrap();
        assert_eq!(thumbprint, key.thumbprint());
        let replay = verify_request_dpop_proof(&secrets, Some(&proof), "POST", url, now, None, None)
            .await
            .unwrap_err();
        assert_eq!(replay.dpop_failure_reason(), Some(DpopFailureReason::Replay));
        assert_eq!(replay.tag(), "ServerAuthInvalidCredentialError");
        let markers: Vec<_> = std::fs::read_dir(dir.path().join("secrets"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(markers.len(), 1);
        assert!(markers[0].starts_with("dpop-proof-") && markers[0].ends_with(".bin"));
        let text = std::fs::read_to_string(dir.path().join("secrets").join(&markers[0])).unwrap();
        assert_eq!(
            text,
            format!(
                "thumbprint={}\njti=jti-1\niat={}\nconsumedAt={}",
                key.thumbprint(),
                now / 1000,
                zc_core::iso_from_millis(now)
            )
        );
    }

    // replayMarkers.test.ts
    #[tokio::test]
    async fn prunes_only_replay_markers_older_than_the_max_age() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_dir = dir.path().join("secrets");
        let secrets = ServerSecretStore::open(&secrets_dir).await.unwrap();
        let now: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z
        let set_age = |file: &str, age_ms: i64| {
            let mtime = SystemTime::UNIX_EPOCH + Duration::from_millis((now - age_ms) as u64);
            let f = std::fs::File::options().write(true).open(secrets_dir.join(file)).unwrap();
            f.set_modified(mtime).unwrap();
        };
        let just_expired = REPLAY_MARKER_MAX_AGE_MS + 1000;
        let expired = [
            "dpop-proof-old",
            "cloud-mint-jti-old",
            "cloud-mint-nonce-old",
            "cloud-health-jti-old",
            "cloud-health-nonce-old",
        ];
        for name in expired {
            secrets.create(name, &[1]).await.unwrap();
            set_age(&format!("{name}.bin"), just_expired);
        }
        secrets.create("dpop-proof-at-max-age", &[1]).await.unwrap();
        set_age("dpop-proof-at-max-age.bin", REPLAY_MARKER_MAX_AGE_MS);
        let real = [
            "server-signing-key",
            "asset-access-signing-key",
            "cloud-relay-url",
            "provider-env-Y29kZXg-T1BFTkFJX0FQSV9LRVk",
        ];
        for name in real {
            secrets.create(name, &[1]).await.unwrap();
            set_age(&format!("{name}.bin"), 30 * REPLAY_MARKER_MAX_AGE_MS);
        }
        let pending = "dpop-proof-pending.bin.0000.tmp";
        std::fs::write(secrets_dir.join(pending), [1]).unwrap();
        set_age(pending, 30 * REPLAY_MARKER_MAX_AGE_MS);

        let removed = prune_expired_replay_markers(&secrets_dir, now).await.unwrap();
        assert_eq!(removed, expired.len());
        let mut remaining: Vec<String> = std::fs::read_dir(&secrets_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        remaining.sort();
        let mut expected: Vec<String> = real.iter().map(|n| format!("{n}.bin")).collect();
        expected.push("dpop-proof-at-max-age.bin".into());
        expected.push(pending.into());
        expected.sort();
        assert_eq!(remaining, expected);
    }
}
