//! `AuthPairingLinkRepository` (`persistence/AuthPairingLinks.ts`): one-time pairing
//! credentials. Consuming one is a single atomic `UPDATE … RETURNING`, so two processes (the
//! server and the CLI) can never both consume it.

use jiff::Timestamp;
use rusqlite::{params, types::Value as SqlValue, Row};
use serde::{Deserialize, Serialize};

use super::auth_sessions::{sql_nullable_text, sql_nullable_timestamp, sql_scopes, sql_text, sql_timestamp};
use super::literal;
use crate::conn::Conn;
use crate::error::{Correlation, DbError, Raw, RawResult, Result};
use crate::time::format_iso;

pub const PAIRING_METHODS: &[&str] = &["desktop-bootstrap", "one-time-token"];

/// `AuthPairingLinkRecord`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthPairingLinkRecord {
    pub id: String,
    pub credential: String,
    /// [`PAIRING_METHODS`].
    pub method: String,
    pub scopes: Vec<String>,
    pub subject: String,
    pub label: Option<String>,
    pub proof_key_thumbprint: Option<String>,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    pub consumed_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

/// `CreateAuthPairingLinkInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAuthPairingLink {
    pub id: String,
    pub credential: String,
    pub method: String,
    pub scopes: Vec<String>,
    pub subject: String,
    pub label: Option<String>,
    pub proof_key_thumbprint: Option<String>,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
}

/// Why a credential could not be consumed (`PairingGrantStore.consume` after
/// `consumeAvailable` found nothing): the TS error each maps to is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumeFailure {
    /// No such credential, or already consumed (`UnknownBootstrapCredentialError`).
    Unknown,
    /// Revoked, or not consumable for another reason (`UnavailableBootstrapCredentialError`).
    Unavailable,
    /// `now >= expiresAt` (`ExpiredBootstrapCredentialError`).
    Expired,
    /// Bound to another proof key (`BootstrapCredentialProofKeyMismatchError`).
    ProofKeyMismatch,
}

fn correlated(operation: &str, correlation: Option<Correlation>, raw: Raw) -> DbError {
    let error = match raw {
        Raw::Sql(error) => match DbError::sql(operation, error) {
            DbError::Sql { operation, kind, cause, .. } => DbError::Sql {
                operation,
                detail: None,
                kind,
                correlation: None,
                cause,
            },
            other => other,
        },
        Raw::Decode(issue) => DbError::decode(operation, issue),
        Raw::Db(error) => return error,
    };
    match correlation {
        Some(correlation) => error.with_correlation(correlation),
        None => error,
    }
}

/// `create`.
pub fn create(conn: &Conn, input: &CreateAuthPairingLink) -> Result<()> {
    let result = (|| -> RawResult<()> {
        literal("method", input.method.clone(), PAIRING_METHODS)?;
        conn.execute(
            r#"
        INSERT INTO auth_pairing_links (
          id,
          credential,
          method,
          scopes,
          subject,
          label,
          proof_key_thumbprint,
          created_at,
          expires_at,
          consumed_at,
          revoked_at
        )
        VALUES (
          ?1,
          ?2,
          ?3,
          ?4,
          ?5,
          ?6,
          ?7,
          ?8,
          ?9,
          NULL,
          NULL
        )
      "#,
            params![
                input.id,
                input.credential,
                input.method,
                serde_json::to_string(&input.scopes).expect("serializing strings cannot fail"),
                input.subject,
                input.label,
                input.proof_key_thumbprint,
                format_iso(input.created_at),
                format_iso(input.expires_at),
            ],
        )?;
        Ok(())
    })();
    result.map_err(|raw| {
        correlated(
            "AuthPairingLinkRepository.create:query",
            Some(Correlation::PairingLinkId(input.id.clone())),
            raw,
        )
    })
}

const LINK_COLUMNS: &str = r#"
          id AS "id",
          credential AS "credential",
          method AS "method",
          scopes AS "scopes",
          subject AS "subject",
          label AS "label",
          proof_key_thumbprint AS "proofKeyThumbprint",
          created_at AS "createdAt",
          expires_at AS "expiresAt",
          consumed_at AS "consumedAt",
          revoked_at AS "revokedAt""#;

struct RawLinkRow {
    id: String,
    values: Vec<SqlValue>,
}

fn raw_link(row: &Row<'_>) -> rusqlite::Result<RawLinkRow> {
    let mut values = Vec::with_capacity(10);
    for index in 1..11 {
        values.push(row.get::<_, SqlValue>(index)?);
    }
    Ok(RawLinkRow { id: row.get(0)?, values })
}

fn decode_link(row: &RawLinkRow) -> RawResult<AuthPairingLinkRecord> {
    let v = &row.values;
    Ok(AuthPairingLinkRecord {
        id: row.id.clone(),
        credential: sql_text("credential", &v[0])?,
        method: literal("method", sql_text("method", &v[1])?, PAIRING_METHODS)?,
        scopes: sql_scopes("scopes", &v[2])?,
        subject: sql_text("subject", &v[3])?,
        label: sql_nullable_text("label", &v[4])?,
        proof_key_thumbprint: sql_nullable_text("proofKeyThumbprint", &v[5])?,
        created_at: sql_timestamp("createdAt", &v[6])?,
        expires_at: sql_timestamp("expiresAt", &v[7])?,
        consumed_at: sql_nullable_timestamp("consumedAt", &v[8])?,
        revoked_at: sql_nullable_timestamp("revokedAt", &v[9])?,
    })
}

fn one_link(conn: &Conn, sql: &str, params: impl rusqlite::Params, query_op: &str, decode_op: &str) -> Result<Option<AuthPairingLinkRecord>> {
    let row = (|| -> RawResult<Option<RawLinkRow>> {
        let mut statement = conn.prepare(sql)?;
        let mut rows = statement.query(params)?;
        match rows.next()? {
            Some(row) => Ok(Some(raw_link(row)?)),
            None => Ok(None),
        }
    })()
    .map_err(|raw| correlated(query_op, None, raw))?;
    match row {
        None => Ok(None),
        Some(row) => decode_link(&row)
            .map(Some)
            .map_err(|raw| correlated(decode_op, Some(Correlation::PairingLinkId(row.id.clone())), raw)),
    }
}

/// `consumeAvailable`: atomically marks the credential consumed if it is unrevoked, unconsumed,
/// unexpired at `now` and not bound to another proof key; returns the consumed link.
pub fn consume_available(
    conn: &Conn,
    credential: &str,
    proof_key_thumbprint: Option<&str>,
    consumed_at: Timestamp,
    now: Timestamp,
) -> Result<Option<AuthPairingLinkRecord>> {
    let sql = format!(
        r#"
        UPDATE auth_pairing_links
        SET consumed_at = ?1
        WHERE credential = ?2
          AND revoked_at IS NULL
          AND consumed_at IS NULL
          AND expires_at > ?3
          AND (
            proof_key_thumbprint IS NULL
            OR proof_key_thumbprint = ?4
          )
        RETURNING{LINK_COLUMNS}
      "#
    );
    one_link(
        conn,
        &sql,
        params![format_iso(consumed_at), credential, format_iso(now), proof_key_thumbprint],
        "AuthPairingLinkRepository.consumeAvailable:query",
        "AuthPairingLinkRepository.consumeAvailable:decodeRow",
    )
}

/// The failure classification `PairingGrantStore.consume` applies when
/// [`consume_available`] returned nothing, from [`get_by_credential`]'s row.
pub fn classify_consume_failure(matching: Option<&AuthPairingLinkRecord>, now: Timestamp, proof_key_thumbprint: Option<&str>) -> ConsumeFailure {
    let Some(link) = matching else {
        return ConsumeFailure::Unknown;
    };
    if link.revoked_at.is_some() {
        return ConsumeFailure::Unavailable;
    }
    if link.consumed_at.is_some() {
        return ConsumeFailure::Unknown;
    }
    if now >= link.expires_at {
        return ConsumeFailure::Expired;
    }
    if let Some(bound) = &link.proof_key_thumbprint {
        if Some(bound.as_str()) != proof_key_thumbprint {
            return ConsumeFailure::ProofKeyMismatch;
        }
    }
    ConsumeFailure::Unavailable
}

/// [`consume_available`], then, if nothing was consumed, [`get_by_credential`] and
/// [`classify_consume_failure`]: the database half of `PairingGrantStore.consume`.
pub fn consume(
    conn: &Conn,
    credential: &str,
    proof_key_thumbprint: Option<&str>,
    now: Timestamp,
) -> Result<std::result::Result<AuthPairingLinkRecord, ConsumeFailure>> {
    if let Some(link) = consume_available(conn, credential, proof_key_thumbprint, now, now)? {
        return Ok(Ok(link));
    }
    let matching = get_by_credential(conn, credential)?;
    Ok(Err(classify_consume_failure(matching.as_ref(), now, proof_key_thumbprint)))
}

/// `listActive({ now })`: unrevoked, unconsumed, unexpired; newest first.
pub fn list_active(conn: &Conn, now: Timestamp) -> Result<Vec<AuthPairingLinkRecord>> {
    let sql = format!(
        r#"
        SELECT{LINK_COLUMNS}
        FROM auth_pairing_links
        WHERE revoked_at IS NULL
          AND consumed_at IS NULL
          AND expires_at > ?1
        ORDER BY created_at DESC, id DESC
      "#
    );
    let rows = (|| -> RawResult<Vec<RawLinkRow>> {
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map(params![format_iso(now)], raw_link)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })()
    .map_err(|raw| correlated("AuthPairingLinkRepository.listActive:query", None, raw))?;
    rows.iter()
        .map(|row| {
            decode_link(row).map_err(|raw| {
                correlated(
                    "AuthPairingLinkRepository.listActive:decodeRows",
                    Some(Correlation::PairingLinkId(row.id.clone())),
                    raw,
                )
            })
        })
        .collect()
}

/// `revoke`: true when an unconsumed, unrevoked link was revoked.
pub fn revoke(conn: &Conn, id: &str, revoked_at: Timestamp) -> Result<bool> {
    let result = (|| -> RawResult<bool> {
        let rows = conn
            .prepare(
                r#"
        UPDATE auth_pairing_links
        SET revoked_at = ?1
        WHERE id = ?2
          AND revoked_at IS NULL
          AND consumed_at IS NULL
        RETURNING id AS "id"
      "#,
            )?
            .query_map(params![format_iso(revoked_at), id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(!rows.is_empty())
    })();
    result.map_err(|raw| correlated("AuthPairingLinkRepository.revoke:query", Some(Correlation::PairingLinkId(id.to_string())), raw))
}

/// `getByCredential`.
pub fn get_by_credential(conn: &Conn, credential: &str) -> Result<Option<AuthPairingLinkRecord>> {
    let sql = format!(
        r#"
        SELECT{LINK_COLUMNS}
        FROM auth_pairing_links
        WHERE credential = ?1
      "#
    );
    one_link(
        conn,
        &sql,
        params![credential],
        "AuthPairingLinkRepository.getByCredential:query",
        "AuthPairingLinkRepository.getByCredential:decodeRow",
    )
}
