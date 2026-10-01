//! OpenCode history (`opencodeUsageReader.ts`): the live SQLite stores (read-only, so WAL
//! writes stay visible) and the pre-migration JSON message files.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use zc_contracts::UsageProviderKind;

use crate::collate::locale_compare;
use crate::json::{self, Sel, J};
use crate::reader::read_dir_sorted;
use crate::records::{int, Totals, UsageRecord};

/// The message fields the parser reads.
const MESSAGE_FIELDS: Sel = Sel::Fields(&[
    ("role", Sel::All),
    ("tokens", Sel::All),
    ("model", Sel::All),
    ("modelID", Sel::All),
    ("time", Sel::Fields(&[("created", Sel::All)])),
    ("id", Sel::All),
    ("sessionID", Sel::All),
    ("cost", Sel::All),
]);

/// `object(value)`: plain objects only (arrays read as `{}`).
fn object(value: Option<&J>) -> Option<&J> {
    value.filter(|value| matches!(value, J::Obj(_)))
}

fn text(value: Option<&J>) -> &str {
    value.and_then(J::as_str).unwrap_or("")
}

#[derive(Debug, Default, Clone)]
struct Fallback {
    id: String,
    session_id: String,
    timestamp_ms: Option<f64>,
}

/// `parseOpenCodeMessage`: uncached input and reasoning are reported apart from input/output.
fn parse_message(source: &[u8], fallback: &Fallback) -> Option<UsageRecord> {
    let parsed = json::parse_selected(source, &MESSAGE_FIELDS)?;
    let message = object(Some(&parsed));
    if let Some(role) = json::get(message, "role") {
        if role.as_str() != Some("assistant") {
            return None;
        }
    }
    let usage = object(json::get(message, "tokens"));
    let cache = object(json::get(usage, "cache"));
    let model_reference = object(json::get(message, "model"));
    let model = [
        text(json::get(model_reference, "id")),
        text(json::get(model_reference, "modelID")),
        text(json::get(message, "modelID")),
    ]
    .into_iter()
    .find(|model| !model.is_empty())
    .unwrap_or("");
    let created = match json::get(object(json::get(message, "time")), "created") {
        None | Some(J::Null) => fallback.timestamp_ms.map(J::Num),
        Some(value) => Some(value.clone()),
    };
    let timestamp_ms = created.as_ref().and_then(J::as_finite);
    if model.is_empty() {
        return None;
    }
    let timestamp_ms = timestamp_ms?;
    let reasoning_tokens = int(json::get(usage, "reasoning"));
    let totals = Totals {
        uncached_input_tokens: int(json::get(usage, "input")),
        cached_input_tokens: int(json::get(cache, "read")),
        cache_creation_tokens: int(json::get(cache, "write")),
        output_tokens: int(json::get(usage, "output")) + reasoning_tokens,
        reasoning_tokens,
    };
    if totals.total() == 0.0 {
        return None;
    }
    let id = if fallback.id.is_empty() {
        text(json::get(message, "id"))
    } else {
        &fallback.id
    };
    let session_id = if fallback.session_id.is_empty() {
        text(json::get(message, "sessionID"))
    } else {
        &fallback.session_id
    };
    Some(UsageRecord {
        provider: UsageProviderKind::Opencode,
        timestamp_ms,
        model: Arc::from(model),
        rate_model: None,
        session_id: Arc::from(session_id),
        totals,
        // OpenCode writes zero for models without a known rate; let the price table estimate.
        reported_cost_usd: json::get(message, "cost").and_then(J::as_finite).filter(|cost| *cost > 0.0),
        fast: false,
        dedupe_key: (!id.is_empty()).then(|| format!("opencode:{id}")),
    })
}

/// One store or legacy file and its records.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub records: Vec<UsageRecord>,
}

/// `OpenCodeUsageReadResult`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenCodeUsage {
    pub files: Vec<SourceFile>,
    pub missing: bool,
    pub error: bool,
}

fn is_store_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("opencode") else {
        return false;
    };
    let Some(rest) = rest.strip_suffix(".db") else {
        return false;
    };
    rest.is_empty() || (rest.len() > 1 && rest.starts_with('-') && rest[1..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'))
}

fn value_text(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
        _ => String::new(),
    }
}

fn value_number(value: ValueRef<'_>) -> Option<f64> {
    match value {
        #[allow(clippy::cast_precision_loss)]
        ValueRef::Integer(integer) => Some(integer as f64),
        ValueRef::Real(real) => Some(real),
        _ => None,
    }
}

/// `append`: in-window records, de-duplicated across every store and file of the root.
struct Collector {
    since_ms: f64,
    seen: HashSet<String>,
}

impl Collector {
    fn append(&mut self, records: &mut Vec<UsageRecord>, record: Option<UsageRecord>) {
        let Some(record) = record else {
            return;
        };
        if record.timestamp_ms < self.since_ms {
            return;
        }
        if let Some(key) = &record.dedupe_key {
            if !self.seen.insert(key.clone()) {
                return;
            }
        }
        records.push(record);
    }
}

fn read_store(path: &Path, collector: &mut Collector, records: &mut Vec<UsageRecord>) -> rusqlite::Result<bool> {
    let since_ms = collector.since_ms;
    let database = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    // A busy live provider fails this source promptly instead of stalling the scan.
    database.busy_timeout(std::time::Duration::from_millis(100))?;
    let tables: HashSet<String> = database
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    let missing_tables = !tables.contains("message") && !tables.contains("session_message");
    for table in ["message", "session_message"] {
        if !tables.contains(table) {
            continue;
        }
        let columns: HashSet<String> = database
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<_, _>>()?;
        let timestamp = if columns.contains("time_created") { "time_created" } else { "NULL" };
        let mut predicates: Vec<&str> = if table == "session_message" { vec!["type = 'assistant'"] } else { Vec::new() };
        if timestamp != "NULL" {
            predicates.push("time_created >= ?");
        }
        let filter = if predicates.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", predicates.join(" AND "))
        };
        let mut statement = database.prepare(&format!("SELECT id, session_id, data, {timestamp} AS created FROM {table}{filter}"))?;
        let mut rows = if timestamp == "NULL" {
            statement.query([])?
        } else if since_ms.fract() == 0.0 && since_ms.abs() < 9e15 {
            #[allow(clippy::cast_possible_truncation)]
            statement.query([since_ms as i64])?
        } else {
            statement.query([since_ms])?
        };
        while let Some(row) = rows.next()? {
            let fallback = Fallback {
                id: value_text(row.get_ref(0)?),
                session_id: value_text(row.get_ref(1)?),
                timestamp_ms: value_number(row.get_ref(3)?),
            };
            let data = match row.get_ref(2)? {
                ValueRef::Text(text) => text.to_vec(),
                _ => Vec::new(),
            };
            collector.append(records, parse_message(&data, &fallback));
        }
    }
    Ok(missing_tables)
}

/// `readOpenCodeUsage`: the stores under `root`, then the legacy JSON messages (store
/// records win over their old JSON copies). Nothing is modified.
pub fn read_opencode_usage(root: &Path, since_ms: f64) -> OpenCodeUsage {
    let mut files: Vec<SourceFile> = Vec::new();
    let mut collector = Collector {
        since_ms,
        seen: HashSet::new(),
    };
    let mut found = false;
    let mut error = false;

    let mut databases: Vec<String> = Vec::new();
    match read_dir_sorted(root) {
        Ok(entries) => {
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type().is_ok_and(|kind| kind.is_file()) && is_store_name(&name) {
                    databases.push(name);
                }
            }
        }
        Err(cause) => {
            if cause.kind() != std::io::ErrorKind::NotFound {
                error = true;
            }
        }
    }
    databases.sort_by(|a, b| {
        if a == "opencode.db" {
            std::cmp::Ordering::Less
        } else if b == "opencode.db" {
            std::cmp::Ordering::Greater
        } else {
            locale_compare(a, b)
        }
    });
    for name in &databases {
        found = true;
        let path = root.join(name);
        let mut records = Vec::new();
        match read_store(&path, &mut collector, &mut records) {
            Ok(missing_tables) => error |= missing_tables,
            Err(_) => error = true,
        }
        files.push(SourceFile { path, records });
    }

    // Symlinks are not followed (cycles included).
    let mut directories = vec![root.join("storage").join("message")];
    while let Some(directory) = directories.pop() {
        let entries = match read_dir_sorted(&directory) {
            Ok(entries) => entries,
            Err(cause) => {
                if cause.kind() != std::io::ErrorKind::NotFound {
                    error = true;
                }
                continue;
            }
        };
        for entry in entries {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if kind.is_dir() {
                directories.push(path);
            } else if kind.is_file() && name.ends_with(".json") {
                found = true;
                let id = &name[..name.len() - 5];
                if collector.seen.contains(&format!("opencode:{id}")) {
                    continue;
                }
                let mut records = Vec::new();
                match std::fs::read(&path) {
                    Ok(source) => collector.append(
                        &mut records,
                        parse_message(
                            &source,
                            &Fallback {
                                id: id.to_owned(),
                                ..Fallback::default()
                            },
                        ),
                    ),
                    Err(cause) => {
                        if cause.kind() != std::io::ErrorKind::NotFound {
                            error = true;
                        }
                    }
                }
                files.push(SourceFile { path, records });
            }
        }
    }
    OpenCodeUsage {
        files,
        missing: !found && !error,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_names() {
        for name in ["opencode.db", "opencode-dev.db", "opencode-a_b-1.db"] {
            assert!(is_store_name(name), "{name}");
        }
        for name in ["opencode-.db", "opencode.db-wal", "other.db", "opencode-a.b.db"] {
            assert!(!is_store_name(name), "{name}");
        }
    }
}
