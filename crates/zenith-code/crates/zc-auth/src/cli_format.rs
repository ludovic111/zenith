//! The output of the `auth` CLI commands (`cliAuthFormat.ts`), byte for byte. The dashboards
//! parse it: `auth pairing create --json` must be a JSON document on its own (they read
//! `.credential`), `auth session issue --json` is read from the first `{` (`token`,
//! `expiresAt`).
//!
//! Each function returns what the TS formatter returns; the command then prints it with
//! `Console.log`, which adds one more newline (see [`console_log`]).

use serde_json::{json, Map, Value};
use zc_contracts::{AuthClientMetadata, AuthClientMetadataDeviceType as DeviceType, AuthClientSession, AuthPairingLink};

use crate::environment_auth::{build_pairing_url, IssuedBearerSession, IssuedPairingLink};

/// `Console.log(text)`: the text and a newline.
pub fn console_log(text: &str) -> String {
    format!("{text}\n")
}

fn iso(millis: i64) -> String {
    zc_core::iso_from_millis(millis)
}

/// `JSON.stringify(value, null, 2)`.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("JSON values always serialize")
}

fn client_json(client: &AuthClientMetadata) -> Value {
    serde_json::to_value(client).expect("client metadata always serializes")
}

/// `formatClientMetadata`.
fn format_client_metadata(metadata: &AuthClientMetadata) -> String {
    let details: Vec<&str> = [
        metadata.label.as_deref(),
        (metadata.device_type != DeviceType::Unknown).then(|| metadata.device_type.as_str()),
        metadata.os.as_deref(),
        metadata.browser.as_deref(),
        metadata.ip_address.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter(|v| !v.is_empty())
    .collect();
    if details.is_empty() {
        "unlabeled client".to_owned()
    } else {
        details.join(" | ")
    }
}

/// `formatIssuedPairingCredential`.
pub fn format_issued_pairing_credential(credential: &IssuedPairingLink, json_output: bool, base_url: Option<&str>) -> String {
    let pair_url = base_url.filter(|u| !u.is_empty()).and_then(|base| {
        let joined = url::Url::parse(base).ok()?.join("/pair").ok()?;
        build_pairing_url(joined.as_str(), &credential.credential)
    });
    if json_output {
        let mut map = Map::new();
        map.insert("id".into(), credential.id.clone().into());
        map.insert("credential".into(), credential.credential.clone().into());
        if let Some(label) = credential.label.as_deref().filter(|l| !l.is_empty()) {
            map.insert("label".into(), label.into());
        }
        map.insert("scopes".into(), credential.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().into());
        map.insert("expiresAt".into(), iso(credential.expires_at).into());
        if let Some(pair_url) = &pair_url {
            map.insert("pairUrl".into(), pair_url.clone().into());
        }
        return format!("{}\n", pretty(&Value::Object(map)));
    }
    let mut lines = vec![
        format!("Issued client pairing token {}.", credential.id),
        format!("Token: {}", credential.credential),
    ];
    if let Some(pair_url) = pair_url {
        lines.push(format!("Pair URL: {pair_url}"));
    }
    // The TS formatter interpolates the DateTime object itself.
    lines.push(format!("Expires at: DateTime.Utc({})", iso(credential.expires_at)));
    format!("{}\n", lines.join("\n"))
}

/// `formatPairingCredentialList`.
pub fn format_pairing_credential_list(credentials: &[AuthPairingLink], json_output: bool) -> String {
    if json_output {
        let items: Vec<Value> = credentials
            .iter()
            .map(|credential| {
                let mut map = Map::new();
                map.insert("id".into(), credential.id.clone().into());
                if let Some(label) = credential.label.as_deref().filter(|l| !l.is_empty()) {
                    map.insert("label".into(), label.into());
                }
                map.insert("scopes".into(), credential.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().into());
                map.insert("createdAt".into(), iso(credential.created_at.as_millis()).into());
                map.insert("expiresAt".into(), iso(credential.expires_at.as_millis()).into());
                Value::Object(map)
            })
            .collect();
        return format!("{}\n", pretty(&Value::Array(items)));
    }
    if credentials.is_empty() {
        return "No active pairing credentials.\n".to_owned();
    }
    let blocks: Vec<String> = credentials
        .iter()
        .map(|credential| {
            let label = credential
                .label
                .as_deref()
                .filter(|l| !l.is_empty())
                .map(|l| format!(" ({l})"))
                .unwrap_or_default();
            [
                format!("{}{label}", credential.id),
                format!("  scopes: {}", credential.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")),
                format!("  created: {}", iso(credential.created_at.as_millis())),
                format!("  expires: {}", iso(credential.expires_at.as_millis())),
            ]
            .join("\n")
        })
        .collect();
    format!("{}\n", blocks.join("\n\n"))
}

/// `formatIssuedSession`.
pub fn format_issued_session(session: &IssuedBearerSession, json_output: bool, token_only: bool) -> String {
    if token_only {
        return format!("{}\n", session.token);
    }
    let scopes: Vec<&str> = session.scopes.iter().map(|s| s.as_str()).collect();
    if json_output {
        let value = json!({
            "sessionId": session.session_id,
            "token": session.token,
            "method": "bearer-access-token",
            "scopes": scopes,
            "subject": session.subject,
            "client": client_json(&session.client),
            "expiresAt": iso(session.expires_at),
        });
        return format!("{}\n", pretty(&value));
    }
    let lines = [
        format!("Issued bearer access token {}.", session.session_id),
        format!("Scopes: {}", scopes.join(" ")),
        format!("Token: {}", session.token),
        format!("Subject: {}", session.subject),
        format!("Client: {}", format_client_metadata(&session.client)),
        format!("Expires at: {}", iso(session.expires_at)),
    ];
    format!("{}\n", lines.join("\n"))
}

/// `formatSessionList`.
pub fn format_session_list(sessions: &[AuthClientSession], json_output: bool) -> String {
    if json_output {
        let items: Vec<Value> = sessions
            .iter()
            .map(|session| {
                json!({
                    "sessionId": session.session_id.as_str(),
                    "method": session.method.as_str(),
                    "scopes": session.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "subject": session.subject,
                    "client": client_json(&session.client),
                    "connected": session.connected,
                    "issuedAt": iso(session.issued_at.as_millis()),
                    "expiresAt": iso(session.expires_at.as_millis()),
                    "lastConnectedAt": session.last_connected_at.map(|t| iso(t.as_millis())),
                })
            })
            .collect();
        return format!("{}\n", pretty(&Value::Array(items)));
    }
    if sessions.is_empty() {
        return "No active sessions.\n".to_owned();
    }
    let blocks: Vec<String> = sessions
        .iter()
        .map(|session| {
            [
                format!("{}{}", session.session_id.as_str(), if session.connected { " connected" } else { "" }),
                format!("  scopes: {}", session.scopes.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")),
                format!("  method: {}", session.method.as_str()),
                format!("  subject: {}", session.subject),
                format!("  client: {}", format_client_metadata(&session.client)),
                format!("  issued: {}", iso(session.issued_at.as_millis())),
                format!(
                    "  last connected: {}",
                    session.last_connected_at.map(|t| iso(t.as_millis())).unwrap_or_else(|| "never".to_owned())
                ),
                format!("  expires: {}", iso(session.expires_at.as_millis())),
            ]
            .join("\n")
        })
        .collect();
    format!("{}\n", blocks.join("\n\n"))
}

/// `auth pairing revoke` output.
pub fn format_pairing_revoke(id: &str, revoked: bool) -> String {
    if revoked {
        format!("Revoked pairing credential {id}.\n")
    } else {
        format!("No active pairing credential found for {id}.\n")
    }
}

/// `auth session revoke` output.
pub fn format_session_revoke(session_id: &str, revoked: bool) -> String {
    if revoked {
        format!("Revoked session {session_id}.\n")
    } else {
        format!("No active session found for {session_id}.\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scopes::STANDARD_CLIENT_SCOPES;
    use zc_contracts::AuthEnvironmentScope as Scope;

    fn link() -> IssuedPairingLink {
        IssuedPairingLink {
            id: "11111111-2222-4333-8444-555555555555".into(),
            credential: "ABCDEFGHJKLM".into(),
            scopes: vec![Scope::OrchestrationRead],
            subject: "one-time-token".into(),
            label: Some("demo".into()),
            created_at: 1_790_000_000_000,
            expires_at: 1_790_000_300_000,
        }
    }

    #[test]
    fn pairing_credential_formats() {
        assert_eq!(
            format_issued_pairing_credential(&link(), true, Some("http://127.0.0.1:3773")),
            "{\n  \"id\": \"11111111-2222-4333-8444-555555555555\",\n  \"credential\": \"ABCDEFGHJKLM\",\n  \"label\": \"demo\",\n  \"scopes\": [\n    \"orchestration:read\"\n  ],\n  \"expiresAt\": \"2026-09-21T14:18:20.000Z\",\n  \"pairUrl\": \"http://127.0.0.1:3773/pair#token=ABCDEFGHJKLM\"\n}\n"
        );
        assert_eq!(
            format_issued_pairing_credential(&link(), false, None),
            "Issued client pairing token 11111111-2222-4333-8444-555555555555.\nToken: ABCDEFGHJKLM\nExpires at: DateTime.Utc(2026-09-21T14:18:20.000Z)\n"
        );
        assert_eq!(format_pairing_credential_list(&[], false), "No active pairing credentials.\n");
        assert_eq!(format_pairing_credential_list(&[], true), "[]\n");
    }

    #[test]
    fn session_formats() {
        let session = IssuedBearerSession {
            session_id: "s1".into(),
            token: "t.k".into(),
            scopes: STANDARD_CLIENT_SCOPES.to_vec(),
            subject: "cli-issued-session".into(),
            client: AuthClientMetadata {
                label: Some("bot".into()),
                ip_address: None,
                user_agent: None,
                device_type: DeviceType::Bot,
                os: None,
                browser: None,
            },
            expires_at: 1_790_000_000_000,
        };
        assert_eq!(format_issued_session(&session, true, true), "t.k\n");
        let json = format_issued_session(&session, true, false);
        assert!(json.starts_with("{\n  \"sessionId\": \"s1\",\n  \"token\": \"t.k\",\n  \"method\": \"bearer-access-token\",\n"));
        assert!(json.contains("\"client\": {\n    \"label\": \"bot\",\n    \"deviceType\": \"bot\"\n  },"));
        assert_eq!(
            format_issued_session(&session, false, false),
            "Issued bearer access token s1.\nScopes: orchestration:read orchestration:operate terminal:operate review:write relay:read\nToken: t.k\nSubject: cli-issued-session\nClient: bot | bot\nExpires at: 2026-09-21T14:13:20.000Z\n"
        );
        assert_eq!(format_session_list(&[], false), "No active sessions.\n");
        assert_eq!(format_session_revoke("x", false), "No active session found for x.\n");
        assert_eq!(format_pairing_revoke("x", true), "Revoked pairing credential x.\n");
    }
}
