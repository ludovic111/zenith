//! `PairingGrantStore` (`auth/PairingGrantStore.ts`): one-time pairing credentials.
//!
//! A credential is 12 characters of `23456789ABCDEFGHJKLMNPQRSTUVWXYZ`, valid 5 minutes by
//! default, stored in plaintext in `auth_pairing_links.credential` and consumed by a single
//! atomic `UPDATE … RETURNING`, so the server and the CLI can never both consume it.
//!
//! The desktop bootstrap grant (seeded in memory from `--bootstrap-fd`) is not ported: zenith
//! runs `serve` on loopback, where only one-time tokens exist.

use rand::RngCore;
use zc_contracts::{AuthEnvironmentScope as Scope, AuthPairingLink, ServerAuthBootstrapMethod};
use zc_core::{PubSub, Subscription};
use zc_db::repos::auth_pairing_links::{self, classify_consume_failure, AuthPairingLinkRecord, ConsumeFailure, CreateAuthPairingLink};
use zc_db::Db;

use crate::clock::SharedClock;
use crate::error::BootstrapCredentialError as E;
use crate::scopes::{parse_scopes, scope_strings, STANDARD_CLIENT_SCOPES};
use crate::session_store::{timestamp, wire_date};

pub const PAIRING_TOKEN_ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
pub const PAIRING_TOKEN_LENGTH: usize = 12;
const PAIRING_TOKEN_REJECTION_LIMIT: usize = (256 / PAIRING_TOKEN_ALPHABET.len()) * PAIRING_TOKEN_ALPHABET.len();
/// 5 minutes.
pub const DEFAULT_ONE_TIME_TOKEN_TTL_MS: i64 = 5 * 60 * 1000;
/// The startup credential of a dev server (`--dev-url`) lives 24 hours.
pub const DEV_STARTUP_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// `generatePairingToken`: rejection sampling over random bytes.
pub fn generate_pairing_token() -> String {
    let mut credential = String::with_capacity(PAIRING_TOKEN_LENGTH);
    let mut bytes = [0u8; PAIRING_TOKEN_LENGTH];
    while credential.len() < PAIRING_TOKEN_LENGTH {
        rand::rng().fill_bytes(&mut bytes);
        for byte in bytes {
            let byte = usize::from(byte);
            if byte >= PAIRING_TOKEN_REJECTION_LIMIT {
                continue;
            }
            credential.push(char::from(PAIRING_TOKEN_ALPHABET[byte % PAIRING_TOKEN_ALPHABET.len()]));
            if credential.len() == PAIRING_TOKEN_LENGTH {
                break;
            }
        }
    }
    credential
}

/// `issueOneTimeToken` input.
#[derive(Clone, Debug, Default)]
pub struct IssueOneTimeTokenInput {
    pub ttl_ms: Option<i64>,
    /// Default the standard client scopes.
    pub scopes: Option<Vec<Scope>>,
    /// Default `one-time-token`.
    pub subject: Option<String>,
    pub label: Option<String>,
    pub proof_key_thumbprint: Option<String>,
    /// The credential the server mints for itself at boot (`purpose: "startup"`).
    pub startup: bool,
}

/// `IssuedBootstrapCredential`.
#[derive(Clone, Debug, PartialEq)]
pub struct IssuedBootstrapCredential {
    pub id: String,
    pub credential: String,
    pub label: Option<String>,
    pub proof_key_thumbprint: Option<String>,
    pub created_at: i64,
    pub expires_at: i64,
}

/// `BootstrapGrant`: what consuming a credential grants.
#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapGrant {
    pub method: ServerAuthBootstrapMethod,
    pub scopes: Vec<Scope>,
    pub subject: String,
    pub label: Option<String>,
    pub proof_key_thumbprint: Option<String>,
    pub expires_at: i64,
}

/// `BootstrapCredentialChange`.
#[derive(Clone, Debug, PartialEq)]
pub enum BootstrapCredentialChange {
    PairingLinkUpserted(AuthPairingLink),
    PairingLinkRemoved(String),
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn to_pairing_link(row: &AuthPairingLinkRecord) -> Result<AuthPairingLink, String> {
    Ok(AuthPairingLink {
        id: row.id.clone(),
        scopes: parse_scopes(&row.scopes).ok_or("scopes: unknown scope")?,
        subject: row.subject.clone(),
        label: non_empty(row.label.clone()),
        created_at: wire_date(row.created_at.as_millisecond()),
        expires_at: wire_date(row.expires_at.as_millisecond()),
    })
}

fn to_grant(row: &AuthPairingLinkRecord) -> Result<BootstrapGrant, String> {
    Ok(BootstrapGrant {
        method: if row.method == "desktop-bootstrap" {
            ServerAuthBootstrapMethod::DesktopBootstrap
        } else {
            ServerAuthBootstrapMethod::OneTimeToken
        },
        scopes: parse_scopes(&row.scopes).ok_or("scopes: unknown scope")?,
        subject: row.subject.clone(),
        label: non_empty(row.label.clone()),
        proof_key_thumbprint: non_empty(row.proof_key_thumbprint.clone()),
        expires_at: row.expires_at.as_millisecond(),
    })
}

/// The `PairingGrantStore` service.
pub struct PairingGrantStore {
    db: Db,
    clock: SharedClock,
    /// A dev URL is configured (the startup credential then lives 24 hours).
    development: bool,
    changes: PubSub<BootstrapCredentialChange>,
}

impl std::fmt::Debug for PairingGrantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PairingGrantStore").finish_non_exhaustive()
    }
}

enum ConsumeOutcome {
    Consumed(Box<AuthPairingLinkRecord>),
    Failed(ConsumeFailure),
}

impl PairingGrantStore {
    pub fn new(db: Db, clock: SharedClock, development: bool) -> Self {
        Self {
            db,
            clock,
            development,
            changes: PubSub::new(),
        }
    }

    /// `streamChanges`.
    pub fn subscribe_changes(&self) -> Subscription<BootstrapCredentialChange> {
        self.changes.subscribe()
    }

    /// `issueOneTimeToken`.
    pub async fn issue_one_time_token(&self, input: IssueOneTimeTokenInput) -> Result<IssuedBootstrapCredential, E> {
        let id = zc_core::uuid_v4();
        let credential = generate_pairing_token();
        let ttl = input.ttl_ms.unwrap_or(if self.development && input.startup {
            DEV_STARTUP_TTL_MS
        } else {
            DEFAULT_ONE_TIME_TOKEN_TTL_MS
        });
        let now = self.clock.now_millis();
        let expires_at = now + ttl;
        let label = non_empty(input.label);
        let proof_key_thumbprint = non_empty(input.proof_key_thumbprint);
        let subject = input.subject.unwrap_or_else(|| "one-time-token".to_owned());
        let scopes = input.scopes.unwrap_or_else(|| STANDARD_CLIENT_SCOPES.to_vec());
        let issue_error = |cause: String| E::PairingCredentialIssue {
            pairing_link_id: id.clone(),
            subject: subject.clone(),
            label: label.clone(),
            cause,
        };
        let record = CreateAuthPairingLink {
            id: id.clone(),
            credential: credential.clone(),
            method: "one-time-token".into(),
            scopes: scope_strings(&scopes),
            subject: subject.clone(),
            label: label.clone(),
            proof_key_thumbprint: proof_key_thumbprint.clone(),
            created_at: timestamp(now).map_err(issue_error)?,
            expires_at: timestamp(expires_at).map_err(issue_error)?,
        };
        self.db
            .call(move |conn| auth_pairing_links::create(conn, &record))
            .await
            .map_err(|e| issue_error(e.to_string()))?;
        self.changes.publish(BootstrapCredentialChange::PairingLinkUpserted(AuthPairingLink {
            id: id.clone(),
            scopes,
            subject,
            label: label.clone(),
            created_at: wire_date(now),
            expires_at: wire_date(expires_at),
        }));
        Ok(IssuedBootstrapCredential {
            id,
            credential,
            label,
            proof_key_thumbprint,
            created_at: now,
            expires_at,
        })
    }

    /// `listActive`: unrevoked, unconsumed, unexpired; newest first. Never the credentials.
    pub async fn list_active(&self) -> Result<Vec<AuthPairingLink>, E> {
        let now = timestamp(self.clock.now_millis()).map_err(|cause| E::ActivePairingLinksLoad { cause })?;
        let rows = self
            .db
            .call(move |conn| auth_pairing_links::list_active(conn, now))
            .await
            .map_err(|e| E::ActivePairingLinksLoad { cause: e.to_string() })?;
        rows.iter()
            .map(to_pairing_link)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|cause| E::ActivePairingLinksLoad { cause })
    }

    /// `revoke`: true when an unconsumed, unrevoked link was revoked.
    pub async fn revoke(&self, id: &str) -> Result<bool, E> {
        let error = |cause: String| E::PairingLinkRevoke {
            pairing_link_id: id.to_owned(),
            cause,
        };
        let at = timestamp(self.clock.now_millis()).map_err(error)?;
        let owned = id.to_owned();
        let revoked = self
            .db
            .call(move |conn| auth_pairing_links::revoke(conn, &owned, at))
            .await
            .map_err(|e| error(e.to_string()))?;
        if revoked {
            self.changes.publish(BootstrapCredentialChange::PairingLinkRemoved(id.to_owned()));
        }
        Ok(revoked)
    }

    /// `consume`: the grant, or why the credential cannot be used.
    pub async fn consume(&self, credential: &str, proof_key_thumbprint: Option<&str>) -> Result<BootstrapGrant, E> {
        let now_ms = self.clock.now_millis();
        let now = timestamp(now_ms).map_err(|cause| E::BootstrapCredentialConsumeAvailable { cause })?;
        let credential = credential.to_owned();
        let thumbprint = proof_key_thumbprint.map(str::to_owned);
        let outcome = self
            .db
            .call(move |conn| {
                let consumed = match auth_pairing_links::consume_available(conn, &credential, thumbprint.as_deref(), now, now) {
                    Ok(consumed) => consumed,
                    Err(e) => return Ok(Err(E::BootstrapCredentialConsumeAvailable { cause: e.to_string() })),
                };
                if let Some(link) = consumed {
                    return Ok(Ok(ConsumeOutcome::Consumed(Box::new(link))));
                }
                let matching = match auth_pairing_links::get_by_credential(conn, &credential) {
                    Ok(matching) => matching,
                    Err(e) => return Ok(Err(E::BootstrapCredentialLookup { cause: e.to_string() })),
                };
                Ok(Ok(ConsumeOutcome::Failed(classify_consume_failure(
                    matching.as_ref(),
                    now,
                    thumbprint.as_deref(),
                ))))
            })
            .await
            .map_err(|e| E::BootstrapCredentialConsume { cause: e.to_string() })??;
        match outcome {
            ConsumeOutcome::Consumed(link) => {
                self.changes.publish(BootstrapCredentialChange::PairingLinkRemoved(link.id.clone()));
                to_grant(&link).map_err(|cause| E::BootstrapCredentialConsumeAvailable { cause })
            }
            ConsumeOutcome::Failed(failure) => Err(match failure {
                ConsumeFailure::Unknown => E::UnknownBootstrapCredential,
                ConsumeFailure::Unavailable => E::UnavailableBootstrapCredential,
                ConsumeFailure::Expired => E::ExpiredBootstrapCredential,
                ConsumeFailure::ProofKeyMismatch => E::BootstrapCredentialProofKeyMismatch,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_tokens_use_the_manual_entry_alphabet() {
        let re = regex::Regex::new("^[23456789ABCDEFGHJKLMNPQRSTUVWXYZ]{12}$").unwrap();
        for _ in 0..200 {
            assert!(re.is_match(&generate_pairing_token()));
        }
    }
}
