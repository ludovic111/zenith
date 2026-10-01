//! `OrchestrationCommandReceiptRepository`: one row per handled command id, so a retried
//! dispatch is answered from the receipt (accepted → same sequence; rejected → error).

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};

use super::{literal, non_negative};
use crate::conn::Conn;
use crate::error::{named_sql, RawResult, Result};

/// `OrchestrationCommandReceipt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandReceipt {
    pub command_id: String,
    /// `"project" | "thread"`.
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub accepted_at: String,
    pub result_sequence: i64,
    /// `"accepted" | "rejected"` (`OrchestrationCommandReceiptStatus`).
    pub status: String,
    pub error: Option<String>,
}

pub const RECEIPT_STATUSES: &[&str] = &["accepted", "rejected"];

/// `upsert`: insert or replace by `command_id`.
pub fn upsert(conn: &Conn, receipt: &CommandReceipt) -> Result<()> {
    let result = conn
        .execute(
            r#"
        INSERT INTO orchestration_command_receipts (
          command_id,
          aggregate_kind,
          aggregate_id,
          accepted_at,
          result_sequence,
          status,
          error
        )
        VALUES (
          ?1,
          ?2,
          ?3,
          ?4,
          ?5,
          ?6,
          ?7
        )
        ON CONFLICT (command_id)
        DO UPDATE SET
          aggregate_kind = excluded.aggregate_kind,
          aggregate_id = excluded.aggregate_id,
          accepted_at = excluded.accepted_at,
          result_sequence = excluded.result_sequence,
          status = excluded.status,
          error = excluded.error
      "#,
            params![
                receipt.command_id,
                receipt.aggregate_kind,
                receipt.aggregate_id,
                receipt.accepted_at,
                receipt.result_sequence,
                receipt.status,
                receipt.error
            ],
        )
        .map(|_| ())
        .map_err(Into::into);
    named_sql(result, "OrchestrationCommandReceiptRepository.upsert:query")
}

fn receipt_from_row(row: &Row<'_>) -> RawResult<CommandReceipt> {
    Ok(CommandReceipt {
        command_id: row.get("commandId")?,
        aggregate_kind: literal("aggregateKind", row.get("aggregateKind")?, &["project", "thread"])?,
        aggregate_id: row.get("aggregateId")?,
        accepted_at: row.get("acceptedAt")?,
        result_sequence: non_negative("resultSequence", row.get("resultSequence")?)?,
        status: literal("status", row.get("status")?, RECEIPT_STATUSES)?,
        error: row.get("error")?,
    })
}

/// `getByCommandId`.
pub fn get_by_command_id(conn: &Conn, command_id: &str) -> Result<Option<CommandReceipt>> {
    let result = (|| -> RawResult<Option<CommandReceipt>> {
        let mut statement = conn.prepare(
            r#"
        SELECT
          command_id AS "commandId",
          aggregate_kind AS "aggregateKind",
          aggregate_id AS "aggregateId",
          accepted_at AS "acceptedAt",
          result_sequence AS "resultSequence",
          status,
          error
        FROM orchestration_command_receipts
        WHERE command_id = ?1
      "#,
        )?;
        let mut rows = statement.query(params![command_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(receipt_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "OrchestrationCommandReceiptRepository.getByCommandId:query")
}
