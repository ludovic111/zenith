//! The durable per-file scan cache, `usage-scan-cache.json` (`usageScanCache.ts`).
//!
//! Transcripts are append-only, so parsed records are cached per file by `(size, mtime)` with
//! the parse position, and a file that grew re-parses only its appended bytes. The on-disk
//! format is the TS one (version 4, interned model and session strings, positional rows), so
//! the TS and Rust servers share the file.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use zc_contracts::UsageProviderKind;

use crate::json::{num, J};
use crate::reader::{ParsePosition, GUARD_LENGTH};
use crate::records::{CodexScanState, Totals, UsageRecord};

/// v4: records carry Claude fast mode.
pub const USAGE_SCAN_CACHE_VERSION: f64 = 4.0;

/// One cached transcript (`CachedFile`).
#[derive(Debug, Clone, PartialEq)]
pub struct CachedFile {
    pub size: f64,
    pub mtime_ms: f64,
    pub provider: UsageProviderKind,
    /// Records of newline-terminated lines, up to `position.resume_offset`.
    pub records: Vec<UsageRecord>,
    /// Records of an unterminated tail, kept apart because a resumed parse re-reads it.
    pub tail_records: Vec<UsageRecord>,
    pub position: ParsePosition,
}

impl CachedFile {
    /// `[...records, ...tailRecords]`.
    pub fn all_records(&self) -> Vec<UsageRecord> {
        let mut all = Vec::with_capacity(self.records.len() + self.tail_records.len());
        all.extend_from_slice(&self.records);
        all.extend_from_slice(&self.tail_records);
        all
    }
}

/// Path → entry, in JS `Map` order (re-setting a key keeps its place).
pub type ScanCache = IndexMap<String, CachedFile>;

fn provider_from(value: &J) -> Option<UsageProviderKind> {
    match value.as_str()? {
        "claude" => Some(UsageProviderKind::Claude),
        "codex" => Some(UsageProviderKind::Codex),
        "grok" => Some(UsageProviderKind::Grok),
        _ => None,
    }
}

fn codex_state_value(state: &CodexScanState) -> Value {
    json!({
        "model": state.model,
        "sessionId": state.session_id,
        "lastUsageSignature": state.last_usage_signature,
        "sawSessionMeta": state.saw_session_meta,
        "suppressingForkCopies": state.suppressing_fork_copies,
        "forkCopyAnchorMs": num(state.fork_copy_anchor_ms),
    })
}

#[derive(Default)]
struct Interner {
    table: Vec<Value>,
    index: HashMap<Arc<str>, usize>,
}

impl Interner {
    fn intern(&mut self, value: &Arc<str>) -> usize {
        if let Some(existing) = self.index.get(value) {
            return *existing;
        }
        let next = self.table.len();
        self.table.push(Value::String(value.to_string()));
        self.index.insert(value.clone(), next);
        next
    }
}

fn encode_row(record: &UsageRecord, models: &mut Interner, sessions: &mut Interner) -> Value {
    Value::Array(vec![
        num(record.timestamp_ms),
        json!(models.intern(&record.model)),
        json!(sessions.intern(&record.session_id)),
        num(record.totals.uncached_input_tokens),
        num(record.totals.cached_input_tokens),
        num(record.totals.cache_creation_tokens),
        num(record.totals.output_tokens),
        num(record.totals.reasoning_tokens),
        record.dedupe_key.clone().map_or(Value::Null, Value::String),
        record.reported_cost_usd.map_or(Value::Null, num),
        json!(u8::from(record.fast)),
    ])
}

/// `encodeScanCache`: interns model and session strings into positional rows.
pub fn encode_scan_cache(cache: &ScanCache) -> Map<String, Value> {
    let mut models = Interner::default();
    let mut sessions = Interner::default();
    let mut files = Map::new();
    for (path, entry) in cache {
        let r: Vec<Value> = entry.records.iter().map(|record| encode_row(record, &mut models, &mut sessions)).collect();
        let t: Vec<Value> = entry.tail_records.iter().map(|record| encode_row(record, &mut models, &mut sessions)).collect();
        let mut file = Map::new();
        file.insert("s".into(), num(entry.size));
        file.insert("m".into(), num(entry.mtime_ms));
        file.insert("p".into(), json!(entry.provider.as_str()));
        file.insert("r".into(), Value::Array(r));
        file.insert("t".into(), Value::Array(t));
        #[allow(clippy::cast_precision_loss)]
        {
            file.insert("o".into(), num(entry.position.resume_offset as f64));
            file.insert("gl".into(), num(entry.position.guard_length as f64));
        }
        file.insert("gh".into(), num(entry.position.guard_hash));
        file.insert("cs".into(), entry.position.codex_state.as_ref().map_or(Value::Null, codex_state_value));
        files.insert(path.clone(), Value::Object(file));
    }
    let mut document = Map::new();
    document.insert("version".into(), json!(4));
    document.insert("models".into(), Value::Array(models.table));
    document.insert("sessions".into(), Value::Array(sessions.table));
    document.insert("files".into(), Value::Object(files));
    document
}

fn finite(value: &J) -> Option<f64> {
    value.as_num().filter(|number| number.is_finite())
}

/// `Number.isSafeInteger`, non-negative.
fn safe_offset(value: &J) -> Option<u64> {
    let number = value.as_num()?;
    if number.fract() != 0.0 || !(0.0..=9_007_199_254_740_991.0).contains(&number) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(number as u64)
}

/// `decodeCodexState`: `Some(None)` for `null`, `None` for a corrupt value.
fn decode_codex_state(value: Option<&J>) -> Option<Option<CodexScanState>> {
    let value = value?;
    if matches!(value, J::Null) {
        return Some(None);
    }
    let state = value.as_obj()?;
    let signature = match state.get("lastUsageSignature") {
        Some(J::Null) => None,
        Some(J::Str(signature)) => Some(signature.clone()),
        _ => return None,
    };
    Some(Some(CodexScanState {
        model: state.get("model")?.as_str()?.to_owned(),
        session_id: state.get("sessionId")?.as_str()?.to_owned(),
        last_usage_signature: signature,
        saw_session_meta: state.get("sawSessionMeta")?.as_bool()?,
        suppressing_fork_copies: state.get("suppressingForkCopies")?.as_bool()?,
        fork_copy_anchor_ms: finite(state.get("forkCopyAnchorMs")?)?,
    }))
}

/// `decodeScanCache`: anything malformed costs a cold parse (an empty cache, or the entry
/// dropped), never a broken page.
pub fn decode_scan_cache(document: &J) -> ScanCache {
    let mut cache = ScanCache::new();
    let Some(root) = document.as_obj() else {
        return cache;
    };
    if root.get("version").and_then(J::as_num) != Some(USAGE_SCAN_CACHE_VERSION) {
        return cache;
    }
    let (Some(models), Some(sessions), Some(files)) = (
        root.get("models").and_then(J::as_arr),
        root.get("sessions").and_then(J::as_arr),
        root.get("files").and_then(J::as_obj),
    ) else {
        return cache;
    };
    // A non-string intern entry rejects the whole cache.
    let Some(models) = models.iter().map(|value| value.as_str().map(Arc::<str>::from)).collect::<Option<Vec<_>>>() else {
        return cache;
    };
    let Some(sessions) = sessions.iter().map(|value| value.as_str().map(Arc::<str>::from)).collect::<Option<Vec<_>>>() else {
        return cache;
    };
    let empty: Arc<str> = Arc::from("");
    let lookup = |table: &[Arc<str>], value: &J| -> Option<Arc<str>> {
        let index = value.as_num()?;
        if index.fract() != 0.0 || index < 0.0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        table.get(index as usize).cloned()
    };
    // Any corrupt row drops the whole entry: keeping the survivors under the same
    // (size, mtime) would read as a valid warm hit and never re-parse.
    let decode_rows = |rows: &[J], provider: UsageProviderKind| -> Option<Vec<UsageRecord>> {
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let row = row.as_arr().filter(|row| row.len() >= 11)?;
            let timestamp_ms = finite(&row[0])?;
            let model = lookup(&models, &row[1])?;
            let totals = Totals {
                uncached_input_tokens: finite(&row[3])?,
                cached_input_tokens: finite(&row[4])?,
                cache_creation_tokens: finite(&row[5])?,
                output_tokens: finite(&row[6])?,
                reasoning_tokens: finite(&row[7])?,
            };
            let fast = match row[10].as_num() {
                Some(0.0) => false,
                Some(1.0) => true,
                _ => return None,
            };
            records.push(UsageRecord {
                provider,
                timestamp_ms,
                model,
                rate_model: None,
                session_id: lookup(&sessions, &row[2]).unwrap_or_else(|| empty.clone()),
                totals,
                reported_cost_usd: row[9].as_num(),
                fast,
                dedupe_key: row[8].as_str().map(str::to_owned),
            });
        }
        Some(records)
    };

    for (path, raw) in files.entries() {
        let Some(entry) = raw.as_obj() else {
            continue;
        };
        let (Some(size), Some(mtime_ms)) = (entry.get("s").and_then(J::as_num), entry.get("m").and_then(J::as_num)) else {
            continue;
        };
        let Some(provider) = entry.get("p").and_then(provider_from) else {
            continue;
        };
        let (Some(rows), Some(tail_rows)) = (entry.get("r").and_then(J::as_arr), entry.get("t").and_then(J::as_arr)) else {
            continue;
        };
        let (Some(resume_offset), Some(guard_length)) = (entry.get("o").and_then(safe_offset), entry.get("gl").and_then(safe_offset)) else {
            continue;
        };
        if guard_length > GUARD_LENGTH || guard_length > resume_offset {
            continue;
        }
        let Some(guard_hash) = entry.get("gh").and_then(finite) else {
            continue;
        };
        let Some(codex_state) = decode_codex_state(entry.get("cs")) else {
            continue;
        };
        let (Some(records), Some(tail_records)) = (decode_rows(rows, provider), decode_rows(tail_rows, provider)) else {
            continue;
        };
        cache.insert(
            path.to_owned(),
            CachedFile {
                size,
                mtime_ms,
                provider,
                records,
                tail_records,
                position: ParsePosition {
                    resume_offset,
                    guard_length,
                    guard_hash,
                    codex_state,
                },
            },
        );
    }
    cache
}

/// `pruneScanCache`: drops entries older than the retention cutoff (saved usage of deleted
/// transcripts is kept until then). Returns how many were removed.
pub fn prune_scan_cache(cache: &mut ScanCache, retention_cutoff_ms: f64) -> usize {
    let before = cache.len();
    cache.retain(|_, entry| entry.mtime_ms >= retention_cutoff_ms);
    before - cache.len()
}

/// `dedupeWithinFile`: keeps the first record per dedupe key; `seen` spans the batches of
/// one file.
pub fn dedupe_within_file(records: Vec<UsageRecord>, seen: &mut HashSet<String>) -> Vec<UsageRecord> {
    records
        .into_iter()
        .filter(|record| match &record.dedupe_key {
            Some(key) => seen.insert(key.clone()),
            None => true,
        })
        .collect()
}

#[cfg(test)]
mod tests;
