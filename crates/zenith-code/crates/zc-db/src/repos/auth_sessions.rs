//! `AuthSessionRepository` (`persistence/AuthSessions.ts`). The CLI (`auth …`) writes the same
//! table from another process.

use jiff::Timestamp;
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Row};
use serde::{Deserialize, Serialize};

use super::literal;
use crate::conn::Conn;
use crate::error::{Correlation, DbError, Raw, RawResult, Result};
use crate::time::{format_iso, parse_iso};

pub const SESSION_METHODS: &[&str] = &["browser-session-cookie", "bearer-access-token", "dpop-access-token"];
pub const DEVICE_TYPES: &[&str] = &["desktop", "mobile", "tablet", "bot", "unknown"];
pub const CLIENT_SURFACES: &[&str] = &["web", "desktop", "mobile", "cli"];

/// `AuthSessionClientMetadataRecord`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientMetadata {
    pub label: Option<String>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
    /// [`DEVICE_TYPES`].
    pub device_type: String,
    pub os: Option<String>,
    pub browser: Option<String>,
}

/// `AuthSessionRecord`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthSessionRecord {
    pub session_id: String,
    pub subject: String,
    /// `AuthEnvironmentScopes` (stored as a JSON array).
    pub scopes: Vec<String>,
    /// [`SESSION_METHODS`].
    pub method: String,
    pub client: ClientMetadata,
    pub issued_at: Timestamp,
    pub expires_at: Timestamp,
    pub last_connected_at: Option<Timestamp>,
    pub revoked_at: Option<Timestamp>,
}

/// `CreateAuthSessionInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateAuthSession {
    pub session_id: String,
    pub subject: String,
    pub scopes: Vec<String>,
    pub method: String,
    pub client: ClientMetadata,
    pub issued_at: Timestamp,
    pub expires_at: Timestamp,
}

fn correlated(operation: &str, correlation: Option<Correlation>, raw: Raw) -> DbError {
    let error = match raw {
        // `new PersistenceSqlError({ operation, correlation, cause })`: no detail.
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

fn scopes_json(scopes: &[String]) -> String {
    serde_json::to_string(scopes).expect("serializing strings cannot fail")
}

fn insert(conn: &Conn, input: &CreateAuthSession, ignore_existing: bool) -> RawResult<()> {
    let conflict = if ignore_existing { "ON CONFLICT(session_id) DO NOTHING" } else { "" };
    let sql = format!(
        r#"
        INSERT INTO auth_sessions (
          session_id,
          subject,
          scopes,
          method,
          client_label,
          client_ip_address,
          client_user_agent,
          client_device_type,
          client_os,
          client_browser,
          issued_at,
          expires_at,
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
          ?10,
          ?11,
          ?12,
          NULL
        )
        {conflict}
      "#
    );
    conn.execute(
        &sql,
        params![
            input.session_id,
            input.subject,
            scopes_json(&input.scopes),
            input.method,
            input.client.label,
            input.client.ip_address,
            input.client.user_agent,
            input.client.device_type,
            input.client.os,
            input.client.browser,
            format_iso(input.issued_at),
            format_iso(input.expires_at),
        ],
    )?;
    Ok(())
}

/// `create`.
pub fn create(conn: &Conn, input: &CreateAuthSession) -> Result<()> {
    insert(conn, input, false).map_err(|raw| {
        correlated(
            "AuthSessionRepository.create:query",
            Some(Correlation::SessionId(input.session_id.clone())),
            raw,
        )
    })
}

/// `createIfAbsent`: a no-op when the session id exists.
pub fn create_if_absent(conn: &Conn, input: &CreateAuthSession) -> Result<()> {
    insert(conn, input, true).map_err(|raw| {
        correlated(
            "AuthSessionRepository.createIfAbsent:query",
            Some(Correlation::SessionId(input.session_id.clone())),
            raw,
        )
    })
}

/// `createReplacingActive`: in one transaction, revoke the subject's live sessions of the same
/// method and create this one. Returns the revoked ids.
pub fn create_replacing_active(conn: &Conn, session: &CreateAuthSession, revoked_at: Timestamp) -> Result<Vec<String>> {
    let result = conn.transaction(|conn| -> RawResult<Vec<String>> {
        let revoked_at = format_iso(revoked_at);
        let revoked = conn
            .prepare(
                r#"
        UPDATE auth_sessions
        SET revoked_at = ?1
        WHERE subject = ?2
          AND method = ?3
          AND revoked_at IS NULL
          AND expires_at > ?1
        RETURNING session_id AS "sessionId"
      "#,
            )?
            .query_map(params![revoked_at, session.subject, session.method], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        insert(conn, session, false)?;
        Ok(revoked)
    });
    result.map_err(|raw| {
        correlated(
            "AuthSessionRepository.createReplacingActive:query",
            Some(Correlation::SessionId(session.session_id.clone())),
            raw,
        )
    })
}

const SESSION_COLUMNS: &str = r#"
          session_id AS "sessionId",
          subject AS "subject",
          scopes AS "scopes",
          method AS "method",
          client_label AS "clientLabel",
          client_ip_address AS "clientIpAddress",
          client_user_agent AS "clientUserAgent",
          client_device_type AS "clientDeviceType",
          client_os AS "clientOs",
          client_browser AS "clientBrowser",
          issued_at AS "issuedAt",
          expires_at AS "expiresAt",
          last_connected_at AS "lastConnectedAt",
          revoked_at AS "revokedAt""#;

/// Raw columns: a row is read before it is decoded, so a decode failure can name its session.
struct RawSessionRow {
    session_id: String,
    values: Vec<SqlValue>,
}

fn raw_session(row: &Row<'_>) -> rusqlite::Result<RawSessionRow> {
    let mut values = Vec::with_capacity(13);
    for index in 1..14 {
        values.push(row.get::<_, SqlValue>(index)?);
    }
    Ok(RawSessionRow {
        session_id: row.get(0)?,
        values,
    })
}

pub(crate) fn sql_text(field: &str, value: &SqlValue) -> RawResult<String> {
    match value {
        SqlValue::Text(text) => Ok(text.clone()),
        _ => Err(Raw::Decode(format!("{field}: InvalidType"))),
    }
}

pub(crate) fn sql_nullable_text(field: &str, value: &SqlValue) -> RawResult<Option<String>> {
    match value {
        SqlValue::Null => Ok(None),
        other => sql_text(field, other).map(Some),
    }
}

pub(crate) fn sql_timestamp(field: &str, value: &SqlValue) -> RawResult<Timestamp> {
    let text = sql_text(field, value)?;
    parse_iso(&text).ok_or_else(|| Raw::Decode(format!("{field}: Encoding(InvalidValue)")))
}

pub(crate) fn sql_nullable_timestamp(field: &str, value: &SqlValue) -> RawResult<Option<Timestamp>> {
    match value {
        SqlValue::Null => Ok(None),
        other => sql_timestamp(field, other).map(Some),
    }
}

pub(crate) fn sql_scopes(field: &str, value: &SqlValue) -> RawResult<Vec<String>> {
    let text = sql_text(field, value)?;
    serde_json::from_str::<Vec<String>>(&text).map_err(|_| Raw::Decode(format!("{field}: Encoding(InvalidValue)")))
}

fn decode_session(row: &RawSessionRow) -> RawResult<AuthSessionRecord> {
    let v = &row.values;
    Ok(AuthSessionRecord {
        session_id: row.session_id.clone(),
        subject: sql_text("subject", &v[0])?,
        scopes: sql_scopes("scopes", &v[1])?,
        method: literal("method", sql_text("method", &v[2])?, SESSION_METHODS)?,
        client: ClientMetadata {
            label: sql_nullable_text("clientLabel", &v[3])?,
            ip_address: sql_nullable_text("clientIpAddress", &v[4])?,
            user_agent: sql_nullable_text("clientUserAgent", &v[5])?,
            device_type: literal("clientDeviceType", sql_text("clientDeviceType", &v[6])?, DEVICE_TYPES)?,
            os: sql_nullable_text("clientOs", &v[7])?,
            browser: sql_nullable_text("clientBrowser", &v[8])?,
        },
        issued_at: sql_timestamp("issuedAt", &v[9])?,
        expires_at: sql_timestamp("expiresAt", &v[10])?,
        last_connected_at: sql_nullable_timestamp("lastConnectedAt", &v[11])?,
        revoked_at: sql_nullable_timestamp("revokedAt", &v[12])?,
    })
}

/// `getById`.
pub fn get_by_id(conn: &Conn, session_id: &str) -> Result<Option<AuthSessionRecord>> {
    let correlation = || Some(Correlation::SessionId(session_id.to_string()));
    let sql = format!(
        r#"
        SELECT{SESSION_COLUMNS}
        FROM auth_sessions
        WHERE session_id = ?1
      "#
    );
    let row = (|| -> RawResult<Option<RawSessionRow>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![session_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(raw_session(row)?)),
            None => Ok(None),
        }
    })()
    .map_err(|raw| correlated("AuthSessionRepository.getById:query", correlation(), raw))?;
    row.as_ref()
        .map(decode_session)
        .transpose()
        .map_err(|raw| correlated("AuthSessionRepository.getById:decodeRow", correlation(), raw))
}

/// `listActive({ now, connectedSessionIds })`: unrevoked sessions that have not expired, or
/// that are connected right now; newest first.
pub fn list_active(conn: &Conn, now: Timestamp, connected_session_ids: &[String]) -> Result<Vec<AuthSessionRecord>> {
    let mut values: Vec<SqlValue> = vec![format_iso(now).into()];
    let connected = if connected_session_ids.is_empty() {
        "1=0".to_string()
    } else {
        values.extend(connected_session_ids.iter().map(|id| SqlValue::from(id.clone())));
        format!("\"session_id\" IN ({})", vec!["?"; connected_session_ids.len()].join(","))
    };
    let sql = format!(
        r#"
        SELECT{SESSION_COLUMNS}
        FROM auth_sessions
        WHERE revoked_at IS NULL
          AND (expires_at > ? OR {connected})
        ORDER BY issued_at DESC, session_id DESC
      "#
    );
    let rows = (|| -> RawResult<Vec<RawSessionRow>> {
        let mut statement = conn.prepare(&sql)?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), raw_session)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })()
    .map_err(|raw| correlated("AuthSessionRepository.listActive:query", None, raw))?;
    rows.iter()
        .map(|row| {
            decode_session(row).map_err(|raw| {
                correlated(
                    "AuthSessionRepository.listActive:decodeRows",
                    Some(Correlation::SessionId(row.session_id.clone())),
                    raw,
                )
            })
        })
        .collect()
}

/// `revoke`: true when the session was live and is now revoked.
pub fn revoke(conn: &Conn, session_id: &str, revoked_at: Timestamp) -> Result<bool> {
    let result = (|| -> RawResult<bool> {
        let rows = conn
            .prepare(
                r#"
        UPDATE auth_sessions
        SET revoked_at = ?1
        WHERE session_id = ?2
          AND revoked_at IS NULL
        RETURNING session_id AS "sessionId"
      "#,
            )?
            .query_map(params![format_iso(revoked_at), session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(!rows.is_empty())
    })();
    result.map_err(|raw| correlated("AuthSessionRepository.revoke:query", Some(Correlation::SessionId(session_id.to_string())), raw))
}

/// `revokeAllExcept`: revokes every other live session; returns their ids.
pub fn revoke_all_except(conn: &Conn, current_session_id: &str, revoked_at: Timestamp) -> Result<Vec<String>> {
    let result = (|| -> RawResult<Vec<String>> {
        Ok(conn
            .prepare(
                r#"
        UPDATE auth_sessions
        SET revoked_at = ?1
        WHERE session_id <> ?2
          AND revoked_at IS NULL
        RETURNING session_id AS "sessionId"
      "#,
            )?
            .query_map(params![format_iso(revoked_at), current_session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    })();
    result.map_err(|raw| {
        correlated(
            "AuthSessionRepository.revokeAllExcept:query",
            Some(Correlation::CurrentSessionId(current_session_id.to_string())),
            raw,
        )
    })
}

/// `setLastConnectedAt` (live sessions only).
pub fn set_last_connected_at(conn: &Conn, session_id: &str, last_connected_at: Timestamp) -> Result<()> {
    let result = conn
        .execute(
            r#"
        UPDATE auth_sessions
        SET last_connected_at = ?1
        WHERE session_id = ?2
          AND revoked_at IS NULL
      "#,
            params![format_iso(last_connected_at), session_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    result.map_err(|raw| {
        correlated(
            "AuthSessionRepository.setLastConnectedAt:query",
            Some(Correlation::SessionId(session_id.to_string())),
            raw,
        )
    })
}

/// `setClientConnection`: the client's surface and app version; a null keeps the stored value
/// (a partial report never erases what a fuller client stored).
pub fn set_client_connection(conn: &Conn, session_id: &str, surface: Option<&str>, app_version: Option<&str>) -> Result<()> {
    let result = (|| -> RawResult<()> {
        if let Some(surface) = surface {
            literal("surface", surface.to_string(), CLIENT_SURFACES)?;
        }
        conn.execute(
            r#"
        UPDATE auth_sessions
        SET client_surface = COALESCE(?1, client_surface),
            client_app_version = COALESCE(?2, client_app_version)
        WHERE session_id = ?3
          AND revoked_at IS NULL
      "#,
            params![surface, app_version, session_id],
        )?;
        Ok(())
    })();
    result.map_err(|raw| {
        correlated(
            "AuthSessionRepository.setClientConnection:query",
            Some(Correlation::SessionId(session_id.to_string())),
            raw,
        )
    })
}
