//! `OrchestrationEventStore` (`persistence/Layers/OrchestrationEventStore.ts`): the append-only
//! event log. `sequence` is global (AUTOINCREMENT) and is what clients cache; `stream_version`
//! is per aggregate and computed inside the INSERT.

use futures::stream::{self, Stream};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{non_negative, parse_json, to_json};
use crate::conn::Conn;
use crate::db::Db;
use crate::error::{named, DbError, Raw, RawResult, Result};

pub const DEFAULT_READ_FROM_SEQUENCE_LIMIT: i64 = 1_000;
pub const READ_PAGE_SIZE: i64 = 500;

/// An event to append (`Omit<OrchestrationEvent, "sequence">`, encoded).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewEvent {
    pub event_id: String,
    /// `"project" | "thread"` (`OrchestrationAggregateKind`).
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub occurred_at: String,
    pub command_id: Option<String>,
    pub causation_event_id: Option<String>,
    pub correlation_id: Option<String>,
    /// `OrchestrationEventMetadata`, encoded.
    pub metadata: Value,
    /// `OrchestrationEventType`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// The event payload, encoded (zc-contracts: the payload union keyed by `type`).
    pub payload: Value,
}

/// A stored event, in the field order of the wire `OrchestrationEvent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistedEvent {
    pub sequence: i64,
    pub event_id: String,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub occurred_at: String,
    pub command_id: Option<String>,
    pub causation_event_id: Option<String>,
    pub correlation_id: Option<String>,
    pub metadata: Value,
    #[serde(rename = "type")]
    pub event_type: String,
    pub payload: Value,
}

/// `OrchestrationAggregateReplayRange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateRange {
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub from_sequence_exclusive: i64,
    pub to_sequence_inclusive: i64,
}

/// `OrchestrationAggregateReplayStats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AggregateReplayStats {
    pub event_count: i64,
    pub payload_bytes: i64,
    /// A creation in this range does not prove that the aggregate still exists.
    pub has_create_event: bool,
}

/// `inferActorKind`: `provider:`/`server:` command ids, then provider metadata, then
/// "no command" is the server, else the client.
pub fn infer_actor_kind(command_id: Option<&str>, metadata: &Value) -> &'static str {
    if let Some(command_id) = command_id {
        if command_id.starts_with("provider:") {
            return "provider";
        }
        if command_id.starts_with("server:") {
            return "server";
        }
    }
    let has = |key: &str| metadata.get(key).is_some();
    if has("providerTurnId") || has("providerItemId") || has("adapterKey") {
        return "provider";
    }
    if command_id.is_none() {
        return "server";
    }
    "client"
}

const EVENT_COLUMNS: &str = r#"
          sequence,
          event_id AS "eventId",
          event_type AS "type",
          aggregate_kind AS "aggregateKind",
          stream_id AS "aggregateId",
          occurred_at AS "occurredAt",
          command_id AS "commandId",
          causation_event_id AS "causationEventId",
          correlation_id AS "correlationId",
          payload_json AS "payload",
          metadata_json AS "metadata""#;

fn event_from_row(row: &Row<'_>) -> RawResult<PersistedEvent> {
    let payload: String = row.get("payload")?;
    let metadata: String = row.get("metadata")?;
    let metadata = parse_json("metadata", &metadata)?;
    if !metadata.is_object() {
        return Err(Raw::Decode("metadata: Encoding(InvalidType)".into()));
    }
    Ok(PersistedEvent {
        sequence: non_negative("sequence", row.get("sequence")?)?,
        event_id: row.get("eventId")?,
        aggregate_kind: row.get("aggregateKind")?,
        aggregate_id: row.get("aggregateId")?,
        occurred_at: row.get("occurredAt")?,
        command_id: row.get("commandId")?,
        causation_event_id: row.get("causationEventId")?,
        correlation_id: row.get("correlationId")?,
        metadata,
        event_type: row.get("type")?,
        payload: parse_json("payload", &payload)?,
    })
}

/// `append`: inserts the event with the next `stream_version` of its aggregate and the inferred
/// `actor_kind`, and returns it with its `sequence`.
pub fn append(conn: &Conn, event: &NewEvent) -> Result<PersistedEvent> {
    let sql = format!(
        r#"
        INSERT INTO orchestration_events (
          event_id,
          aggregate_kind,
          stream_id,
          stream_version,
          event_type,
          occurred_at,
          command_id,
          causation_event_id,
          correlation_id,
          actor_kind,
          payload_json,
          metadata_json
        )
        VALUES (
          ?1,
          ?2,
          ?3,
          COALESCE(
            (
              SELECT stream_version + 1
              FROM orchestration_events
              WHERE aggregate_kind = ?2
                AND stream_id = ?3
              ORDER BY stream_version DESC
              LIMIT 1
            ),
            0
          ),
          ?4,
          ?5,
          ?6,
          ?7,
          ?8,
          ?9,
          ?10,
          ?11
        )
        RETURNING{EVENT_COLUMNS}
      "#
    );
    let actor_kind = infer_actor_kind(event.command_id.as_deref(), &event.metadata);
    let result = (|| -> RawResult<PersistedEvent> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![
            event.event_id,
            event.aggregate_kind,
            event.aggregate_id,
            event.event_type,
            event.occurred_at,
            event.command_id,
            event.causation_event_id,
            event.correlation_id,
            actor_kind,
            to_json(&event.payload),
            to_json(&event.metadata),
        ])?;
        let row = rows.next()?.ok_or_else(|| Raw::Decode("NoSuchElement".into()))?;
        event_from_row(row)
    })();
    named(result, "OrchestrationEventStore.append:insert", "OrchestrationEventStore.append:decodeRow")
}

fn read_page_from_sequence(conn: &Conn, sequence_exclusive: i64, limit: i64) -> Result<Vec<PersistedEvent>> {
    let sql = format!(
        r#"
        SELECT{EVENT_COLUMNS}
        FROM orchestration_events
        WHERE sequence > ?1
        ORDER BY sequence ASC
        LIMIT ?2
      "#
    );
    let result = (|| -> RawResult<Vec<PersistedEvent>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![sequence_exclusive, limit])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(event_from_row(row)?);
        }
        Ok(events)
    })();
    named(
        result,
        "OrchestrationEventStore.readFromSequence:query",
        "OrchestrationEventStore.readFromSequence:decodeRows",
    )
}

fn read_aggregate_page(conn: &Conn, range: &AggregateRange, from_exclusive: i64, limit: i64) -> Result<Vec<PersistedEvent>> {
    let sql = format!(
        r#"
        SELECT{EVENT_COLUMNS}
        FROM orchestration_events
        WHERE aggregate_kind = ?1
          AND stream_id = ?2
          AND sequence > ?3
          AND sequence <= ?4
        ORDER BY sequence ASC
        LIMIT ?5
      "#
    );
    let result = (|| -> RawResult<Vec<PersistedEvent>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![
            range.aggregate_kind,
            range.aggregate_id,
            from_exclusive,
            range.to_sequence_inclusive,
            limit
        ])?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            events.push(event_from_row(row)?);
        }
        Ok(events)
    })();
    named(
        result,
        "OrchestrationEventStore.readAggregateRange:query",
        "OrchestrationEventStore.readAggregateRange:decodeRows",
    )
}

/// Page-by-page reader behind `readFromSequence` and `readAggregateRange` (Effect's
/// `Stream.paginate`): each page is one query of at most [`READ_PAGE_SIZE`] rows, and only one
/// page is held at a time.
#[derive(Debug, Clone)]
pub struct EventPager {
    mode: PagerMode,
    cursor: i64,
    remaining: i64,
    done: bool,
}

#[derive(Debug, Clone)]
enum PagerMode {
    All,
    Aggregate(AggregateRange),
}

impl EventPager {
    /// `readFromSequence(sequenceExclusive, limit = 1000)`. A limit ≤ 0 reads nothing.
    pub fn from_sequence(sequence_exclusive: i64, limit: Option<i64>) -> Self {
        let limit = limit.unwrap_or(DEFAULT_READ_FROM_SEQUENCE_LIMIT).max(0);
        Self {
            mode: PagerMode::All,
            cursor: sequence_exclusive,
            remaining: limit,
            done: limit == 0,
        }
    }

    /// `readAll()`: every event (`readFromSequence(0, Number.MAX_SAFE_INTEGER)`).
    pub fn all() -> Self {
        Self::from_sequence(0, Some(9_007_199_254_740_991))
    }

    /// `readAggregateRange`: one aggregate between two sequences (the upper one is a head the
    /// caller captured, so events appended meanwhile are not read).
    pub fn aggregate_range(range: AggregateRange, limit: Option<i64>) -> Self {
        let limit = limit.unwrap_or(DEFAULT_READ_FROM_SEQUENCE_LIMIT).max(0);
        let done = limit == 0 || range.from_sequence_exclusive >= range.to_sequence_inclusive;
        Self {
            cursor: range.from_sequence_exclusive,
            mode: PagerMode::Aggregate(range),
            remaining: limit,
            done,
        }
    }

    /// The next page, or `None` when the read is over.
    pub fn next_page(&mut self, conn: &Conn) -> Result<Option<Vec<PersistedEvent>>> {
        if self.done {
            return Ok(None);
        }
        let page_limit = self.remaining.min(READ_PAGE_SIZE);
        let events = match &self.mode {
            PagerMode::All => read_page_from_sequence(conn, self.cursor, page_limit)?,
            PagerMode::Aggregate(range) => read_aggregate_page(conn, range, self.cursor, page_limit)?,
        };
        let next_remaining = self.remaining - events.len() as i64;
        match (&self.mode, events.last()) {
            (_, None) => self.done = true,
            (PagerMode::All, Some(last)) => {
                self.done = next_remaining <= 0;
                self.cursor = last.sequence;
            }
            (PagerMode::Aggregate(range), Some(last)) => {
                self.done = (events.len() as i64) < READ_PAGE_SIZE || next_remaining == 0 || last.sequence >= range.to_sequence_inclusive;
                self.cursor = last.sequence;
            }
        }
        self.remaining = next_remaining;
        Ok(Some(events))
    }

    /// Reads every remaining page into one vector.
    pub fn collect(mut self, conn: &Conn) -> Result<Vec<PersistedEvent>> {
        let mut all = Vec::new();
        while let Some(page) = self.next_page(conn)? {
            all.extend(page);
        }
        Ok(all)
    }

    /// The pages as an async stream of events, each page one call on the writer.
    pub fn into_stream(self, db: Db) -> impl Stream<Item = Result<PersistedEvent>> + Send {
        stream::unfold(Some((self, db)), |state| async move {
            let (pager, db) = state?;
            let mut moved = pager;
            let result = db
                .call(move |conn| {
                    let page = moved.next_page(conn)?;
                    Ok((moved, page))
                })
                .await;
            match result {
                Ok((pager, Some(page))) => Some((Ok::<_, DbError>(page), Some((pager, db)))),
                Ok((_, None)) => None,
                Err(error) => Some((Err(error), None)),
            }
        })
        .flat_map(|page| {
            stream::iter(match page {
                Ok(events) => events.into_iter().map(Ok).collect::<Vec<_>>(),
                Err(error) => vec![Err(error)],
            })
        })
    }
}

use futures::StreamExt;

/// `readFromSequence`, collected.
pub fn read_from_sequence(conn: &Conn, sequence_exclusive: i64, limit: Option<i64>) -> Result<Vec<PersistedEvent>> {
    EventPager::from_sequence(sequence_exclusive, limit).collect(conn)
}

/// `readAll`, collected. Prefer [`EventPager::all`] with `into_stream` on a large log.
pub fn read_all(conn: &Conn) -> Result<Vec<PersistedEvent>> {
    EventPager::all().collect(conn)
}

/// `readAggregateRange`, collected.
pub fn read_aggregate_range(conn: &Conn, range: &AggregateRange, limit: Option<i64>) -> Result<Vec<PersistedEvent>> {
    EventPager::aggregate_range(range.clone(), limit).collect(conn)
}

/// `getAggregateReplayStats`: counts at most `max_events + 1` rows of the range and their
/// payload bytes (UTF-8, `octet_length`) without decoding them.
pub fn get_aggregate_replay_stats(conn: &Conn, range: &AggregateRange, max_events: i64) -> Result<AggregateReplayStats> {
    let limit = max_events.max(0) + 1;
    let result = (|| -> RawResult<AggregateReplayStats> {
        let mut statement = conn.prepare(
            r#"
        SELECT
          COUNT(*) AS "eventCount",
          COALESCE(SUM(octet_length(payload_json)), 0) AS "payloadBytes",
          COALESCE(MAX(event_type IN (
            'thread.created', 'project.created'
          )), 0) AS "hasCreateEvent"
        FROM (
          SELECT payload_json, event_type
          FROM orchestration_events
          WHERE aggregate_kind = ?1
            AND stream_id = ?2
            AND sequence > ?3
            AND sequence <= ?4
          ORDER BY sequence ASC
          LIMIT ?5
        )
      "#,
        )?;
        let stats = statement.query_row(
            params![
                range.aggregate_kind,
                range.aggregate_id,
                range.from_sequence_exclusive,
                range.to_sequence_inclusive,
                limit
            ],
            |row| {
                Ok(AggregateReplayStats {
                    event_count: row.get(0)?,
                    payload_bytes: row.get(1)?,
                    has_create_event: row.get::<_, i64>(2)? != 0,
                })
            },
        )?;
        Ok(stats)
    })();
    named(
        result,
        "OrchestrationEventStore.getAggregateReplayStats:query",
        "OrchestrationEventStore.getAggregateReplayStats:decodeRow",
    )
}

/// `hasEventAfter`: whether the aggregate has an event after `sequence_exclusive`, optionally of
/// one type.
pub fn has_event_after(conn: &Conn, aggregate_kind: &str, aggregate_id: &str, event_type: Option<&str>, sequence_exclusive: i64) -> Result<bool> {
    let result = (|| -> RawResult<bool> {
        let found = match event_type {
            None => conn
                .prepare(
                    r#"
          SELECT sequence
          FROM orchestration_events
          WHERE aggregate_kind = ?1
            AND stream_id = ?2
            AND (sequence > ?3)
          LIMIT 1
        "#,
                )?
                .query_row(params![aggregate_kind, aggregate_id, sequence_exclusive], |row| row.get::<_, i64>(0))
                .optional()?,
            Some(event_type) => conn
                .prepare(
                    r#"
          SELECT sequence
          FROM orchestration_events
          WHERE aggregate_kind = ?1
            AND stream_id = ?2
            AND (sequence > ?3 AND event_type = ?4)
          LIMIT 1
        "#,
                )?
                .query_row(params![aggregate_kind, aggregate_id, sequence_exclusive, event_type], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?,
        };
        Ok(found.is_some())
    })();
    named(
        result,
        "OrchestrationEventStore.hasEventAfter:query",
        "OrchestrationEventStore.hasEventAfter:decodeRow",
    )
}

/// The highest stored sequence (0 for an empty log).
pub fn latest_sequence(conn: &Conn) -> Result<i64> {
    conn.prepare("SELECT COALESCE(MAX(sequence), 0) FROM orchestration_events")
        .and_then(|mut statement| statement.query_row([], |row| row.get(0)))
        .map_err(|error| DbError::sql("OrchestrationEventStore.latestSequence:query", error))
}
