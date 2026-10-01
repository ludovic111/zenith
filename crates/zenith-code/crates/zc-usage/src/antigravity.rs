//! Antigravity history (`antigravityUsageReader.ts`): per-conversation SQLite stores whose
//! usage metadata is protobuf, merged across stores by response/provider/message ids.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use zc_contracts::UsageProviderKind;

use crate::collate::locale_compare;
use crate::json;
use crate::reader::mtime_ms;
use crate::records::{Totals, UsageRecord};

#[derive(Debug)]
pub struct DecodeError;

type R<T> = Result<T, DecodeError>;

/// One protobuf field value: a varint (`Big` above 2^53 - 1, which reads as 0 like a
/// bigint does in TS) or a length-delimited payload. Fixed-width fields are skipped.
#[derive(Debug, Clone, Copy)]
enum FieldValue<'a> {
    Num(f64),
    Big,
    Bytes(&'a [u8]),
}

type Fields<'a> = HashMap<u64, Vec<FieldValue<'a>>>;

const MAX_SAFE: u64 = 9_007_199_254_740_991;

fn varint(bytes: &[u8], offset: &mut usize) -> R<FieldValue<'static>> {
    let mut value: u64 = 0;
    let mut shift = 0;
    while shift < 70 {
        let byte = *bytes.get(*offset).ok_or(DecodeError)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(DecodeError);
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            #[allow(clippy::cast_precision_loss)]
            return Ok(if value > MAX_SAFE { FieldValue::Big } else { FieldValue::Num(value as f64) });
        }
        shift += 7;
    }
    Err(DecodeError)
}

fn fields(bytes: &[u8]) -> R<Fields<'_>> {
    let mut offset = 0;
    let mut result: Fields<'_> = HashMap::new();
    while offset < bytes.len() {
        let FieldValue::Num(tag) = varint(bytes, &mut offset)? else {
            return Err(DecodeError);
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let tag = tag as u64;
        let number = tag / 8;
        let wire = tag % 8;
        if number == 0 {
            return Err(DecodeError);
        }
        let value = match wire {
            0 => varint(bytes, &mut offset)?,
            1 | 2 | 5 => {
                let length = match wire {
                    2 => match varint(bytes, &mut offset)? {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        FieldValue::Num(length) => length as usize,
                        _ => return Err(DecodeError),
                    },
                    1 => 8,
                    _ => 4,
                };
                if length > bytes.len() - offset {
                    return Err(DecodeError);
                }
                let value = &bytes[offset..offset + length];
                offset += length;
                if wire != 2 {
                    continue;
                }
                FieldValue::Bytes(value)
            }
            _ => return Err(DecodeError),
        };
        result.entry(number).or_default().push(value);
    }
    Ok(result)
}

fn number_at(value: &Fields<'_>, key: u64) -> f64 {
    match value.get(&key).and_then(|entries| entries.first()) {
        Some(FieldValue::Num(number)) => *number,
        _ => 0.0,
    }
}

fn bytes_at<'a>(value: &Fields<'a>, key: u64) -> Option<&'a [u8]> {
    match value.get(&key).and_then(|entries| entries.first()) {
        Some(FieldValue::Bytes(bytes)) => Some(bytes),
        _ => None,
    }
}

fn nested<'a>(value: &Fields<'a>, key: u64) -> R<Fields<'a>> {
    match bytes_at(value, key) {
        Some(bytes) => fields(bytes),
        None => Ok(HashMap::new()),
    }
}

/// UTF-8 (fatal), trimmed.
fn text_at(value: &Fields<'_>, key: u64) -> R<String> {
    match bytes_at(value, key) {
        Some(bytes) => Ok(json::js_trim(std::str::from_utf8(bytes).map_err(|_| DecodeError)?).to_owned()),
        None => Ok(String::new()),
    }
}

fn timestamp(value: &Fields<'_>) -> Option<f64> {
    let seconds = number_at(value, 1);
    (seconds > 0.0).then(|| seconds * 1000.0 + (number_at(value, 2) / 1_000_000.0).floor())
}

fn model_id(id: f64) -> Option<&'static str> {
    #[allow(clippy::cast_possible_truncation)]
    let id = if id.fract() == 0.0 { id as i64 } else { return None };
    Some(match id {
        246 => "gemini-2.5-pro",
        312 => "gemini-2.5-flash",
        313 | 329 => "gemini-2.5-flash-thinking",
        330 => "gemini-2.5-flash-lite",
        281 | 282 => "claude-sonnet-4",
        290 | 291 => "claude-opus-4",
        333 | 334 => "claude-sonnet-4-5",
        340 | 341 => "claude-haiku-4-5",
        1026 => "claude-opus-4-6",
        1035 => "claude-sonnet-4-6",
        1016 | 1036 | 1037 => "gemini-3.1-pro",
        1018 | 1084 | 1047 => "gemini-3-flash-preview",
        _ => return None,
    })
}

static TRAILING_PARENTHETICAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s*\([^)]*\)\s*$").expect("valid regex"));
static CLAUDE_VERSION_FIRST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^claude-(4(?:\.[0-9]+)?)-(sonnet|opus|haiku)").expect("valid regex"));

/// `modelName`: a display name normalized to a rate-table id, else the numeric model id.
fn model_name(name: &str, id: f64) -> String {
    if !name.is_empty() {
        let lowered = name.to_lowercase();
        let normalized = TRAILING_PARENTHETICAL.replace(&lowered, "").replace(' ', "-");
        if normalized.starts_with("claude-") {
            return CLAUDE_VERSION_FIRST.replace(&normalized, "claude-$2-$1").replace('.', "-");
        }
        return normalized;
    }
    match model_id(id) {
        Some(model) => model.to_owned(),
        None if id > 0.0 => format!("antigravity-model-{}", json::number_to_string(id)),
        None => String::new(),
    }
}

struct Metadata<'a> {
    model: String,
    timestamp_ms: Option<f64>,
    usages: Vec<Fields<'a>>,
}

fn metadata(bytes: &[u8], step: bool) -> R<Metadata<'_>> {
    let root = fields(bytes)?;
    if !step && bytes_at(&root, 1).is_none() {
        return Err(DecodeError);
    }
    let data = if step { root } else { nested(&root, 1)? };
    let model = if step { nested(&data, 24)? } else { data.clone() };
    let mut usages = Vec::new();
    if let Some(usage) = bytes_at(&data, if step { 9 } else { 4 }) {
        usages.push(fields(usage)?);
    }
    for retry in data.get(&if step { 28 } else { 17 }).map(Vec::as_slice).unwrap_or_default() {
        let FieldValue::Bytes(retry) = retry else {
            return Err(DecodeError);
        };
        if let Some(retry_usage) = bytes_at(&fields(retry)?, 2) {
            usages.push(fields(retry_usage)?);
        }
    }
    let mut name = text_at(&model, if step { 12 } else { 19 })?;
    if name.is_empty() {
        name = text_at(&model, if step { 8 } else { 21 })?;
    }
    let model_name = model_name(&name, number_at(&model, if step { 1 } else { 3 }));
    let timestamp_ms = if step {
        match timestamp(&nested(&data, 8)?) {
            Some(timestamp) => Some(timestamp),
            None => timestamp(&nested(&data, 1)?),
        }
    } else {
        timestamp(&nested(&nested(&data, 9)?, 4)?)
    };
    Ok(Metadata {
        model: model_name,
        timestamp_ms,
        usages,
    })
}

#[derive(Debug, Clone)]
struct Candidate {
    record: UsageRecord,
    keys: Vec<String>,
    timestamp_quality: u8,
}

#[derive(Debug)]
enum StoreError {
    Sql,
    Decode,
}

impl From<rusqlite::Error> for StoreError {
    fn from(_: rusqlite::Error) -> Self {
        StoreError::Sql
    }
}

impl From<DecodeError> for StoreError {
    fn from(_: DecodeError) -> Self {
        StoreError::Decode
    }
}

fn blob(value: ValueRef<'_>) -> Result<Vec<u8>, StoreError> {
    match value {
        ValueRef::Blob(bytes) => Ok(bytes.to_vec()),
        _ => Err(StoreError::Decode),
    }
}

fn read_metadata_rows(db: &Connection, query: &str) -> Result<Vec<(f64, Vec<u8>)>, StoreError> {
    let mut statement = db.prepare(query)?;
    let mut rows = statement.query([])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        #[allow(clippy::cast_precision_loss)]
        let idx = match row.get_ref(0)? {
            ValueRef::Integer(integer) => integer as f64,
            ValueRef::Real(real) => real,
            _ => return Err(StoreError::Decode),
        };
        entries.push((idx, blob(row.get_ref(1)?)?));
    }
    Ok(entries)
}

/// The key of a JS `Map` keyed by a number (`-0` and `0` are one key).
fn number_key(value: f64) -> u64 {
    if value == 0.0 {
        0
    } else {
        value.to_bits()
    }
}

fn read_database(path: &Path, fallback_timestamp: f64) -> Result<Vec<Candidate>, StoreError> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    db.busy_timeout(std::time::Duration::from_millis(100))?;
    db.execute_batch("BEGIN")?;
    let tables: HashSet<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    if !tables.contains("gen_metadata") && !tables.contains("steps") {
        return Err(StoreError::Decode);
    }
    let generation_rows = if tables.contains("gen_metadata") {
        read_metadata_rows(&db, "SELECT idx, data FROM gen_metadata ORDER BY idx")?
    } else {
        Vec::new()
    };
    let generations: Vec<(f64, Metadata<'_>)> = generation_rows
        .iter()
        .map(|(idx, bytes)| Ok((*idx, metadata(bytes, false)?)))
        .collect::<R<_>>()?;
    let mut trajectory_timestamp: Option<f64> = None;
    if tables.contains("trajectory_metadata_blob") {
        let mut statement = db.prepare("SELECT data FROM trajectory_metadata_blob")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            if trajectory_timestamp.is_none() {
                let bytes = blob(row.get_ref(0)?)?;
                trajectory_timestamp = timestamp(&nested(&fields(&bytes)?, 2)?);
            }
        }
    }
    let step_rows = if tables.contains("steps") {
        read_metadata_rows(&db, "SELECT idx, metadata FROM steps WHERE metadata IS NOT NULL ORDER BY idx")?
    } else {
        Vec::new()
    };
    let steps: Vec<(f64, Metadata<'_>)> = step_rows.iter().map(|(idx, bytes)| Ok((*idx, metadata(bytes, true)?))).collect::<R<_>>()?;

    let session_id: Arc<str> = Arc::from(
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .map(|name| name.strip_suffix(".db").map(str::to_owned).unwrap_or(name))
            .unwrap_or_default(),
    );
    let mut generation_models: HashMap<u64, &str> = HashMap::new();
    for (idx, entry) in &generations {
        generation_models.insert(number_key(*idx), &entry.model);
    }
    let mut records = Vec::new();
    for (source, entries) in [("step", &steps), ("generation", &generations)] {
        for (index, (idx, entry)) in entries.iter().enumerate() {
            for (usage_index, usage) in entry.usages.iter().enumerate() {
                let output_tokens = f64::max(number_at(usage, 3), number_at(usage, 9) + number_at(usage, 10));
                let totals = Totals {
                    uncached_input_tokens: number_at(usage, 2),
                    cached_input_tokens: number_at(usage, 5),
                    cache_creation_tokens: number_at(usage, 4),
                    output_tokens,
                    reasoning_tokens: f64::min(output_tokens, number_at(usage, 9)),
                };
                if totals.uncached_input_tokens + totals.cached_input_tokens + totals.cache_creation_tokens + output_tokens == 0.0 {
                    continue;
                }
                let mut keys = Vec::new();
                for key in [11, 12, 7] {
                    let id = text_at(usage, key)?;
                    if !id.is_empty() {
                        keys.push(format!("antigravity:{key}:{id}"));
                    }
                }
                let usage_model_id = number_at(usage, 1);
                let generation_model = if source == "step" {
                    generation_models.get(&number_key(*idx)).copied().unwrap_or("")
                } else {
                    ""
                };
                let model = [model_id(usage_model_id).unwrap_or(""), entry.model.as_str(), generation_model]
                    .into_iter()
                    .find(|model| !model.is_empty())
                    .map(str::to_owned)
                    .or_else(|| Some(model_name("", usage_model_id)).filter(|model| !model.is_empty()))
                    .unwrap_or_else(|| "antigravity-unknown".to_owned());
                let dedupe_key = keys
                    .first()
                    .cloned()
                    .unwrap_or_else(|| format!("antigravity:{session_id}:{source}:{index}:{usage_index}"));
                records.push(Candidate {
                    record: UsageRecord {
                        provider: UsageProviderKind::Antigravity,
                        timestamp_ms: entry.timestamp_ms.or(trajectory_timestamp).unwrap_or(fallback_timestamp),
                        model: Arc::from(model),
                        rate_model: None,
                        session_id: session_id.clone(),
                        totals,
                        reported_cost_usd: None,
                        fast: false,
                        dedupe_key: Some(dedupe_key),
                    },
                    keys,
                    timestamp_quality: if entry.timestamp_ms.is_some() {
                        2
                    } else if trajectory_timestamp.is_some() {
                        1
                    } else {
                        0
                    },
                });
            }
        }
    }
    Ok(records)
}

/// One store and the records it owns after merging.
#[derive(Debug, Clone, PartialEq)]
pub struct AntigravityFile {
    pub root: PathBuf,
    pub path: PathBuf,
    pub records: Vec<UsageRecord>,
}

/// `readAntigravityUsage`'s result: stores in walk order, and the paths that failed.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AntigravityUsage {
    pub files: Vec<AntigravityFile>,
    pub errors: Vec<PathBuf>,
}

struct Group {
    candidate: Candidate,
    parent: usize,
    size: usize,
    owner: usize,
    file_index: usize,
}

struct Merger {
    groups: Vec<Group>,
    identities: HashMap<String, usize>,
}

impl Merger {
    fn find(&mut self, mut index: usize) -> usize {
        let mut root = index;
        while self.groups[root].parent != root {
            root = self.groups[root].parent;
        }
        while index != root {
            let parent = self.groups[index].parent;
            self.groups[index].parent = root;
            index = parent;
        }
        root
    }

    fn merge(&mut self, left: usize, right: usize) -> usize {
        let mut a = self.find(left);
        let mut b = self.find(right);
        if a == b {
            return a;
        }
        if self.groups[a].size < self.groups[b].size {
            std::mem::swap(&mut a, &mut b);
        }
        let (target, source) = (&self.groups[a], &self.groups[b]);
        let first_is_target = target.owner < source.owner;
        let first = if first_is_target { target } else { source };
        let best_time = if source.candidate.timestamp_quality > target.candidate.timestamp_quality
            || (source.candidate.timestamp_quality == target.candidate.timestamp_quality
                && source.candidate.record.timestamp_ms < target.candidate.record.timestamp_ms)
        {
            source
        } else {
            target
        };
        let x = target.candidate.record.totals;
        let y = source.candidate.record.totals;
        let mut record = first.candidate.record.clone();
        if &*first.candidate.record.model == "antigravity-unknown" {
            record.model = if first_is_target {
                source.candidate.record.model.clone()
            } else {
                target.candidate.record.model.clone()
            };
        }
        record.timestamp_ms = best_time.candidate.record.timestamp_ms;
        record.totals = Totals {
            uncached_input_tokens: f64::max(x.uncached_input_tokens, y.uncached_input_tokens),
            cached_input_tokens: f64::max(x.cached_input_tokens, y.cached_input_tokens),
            cache_creation_tokens: f64::max(x.cache_creation_tokens, y.cache_creation_tokens),
            output_tokens: f64::max(x.output_tokens, y.output_tokens),
            reasoning_tokens: f64::max(x.reasoning_tokens, y.reasoning_tokens),
        };
        let timestamp_quality = best_time.candidate.timestamp_quality;
        let owner = first.owner;
        let file_index = first.file_index;
        let source_size = source.size;
        let target = &mut self.groups[a];
        target.candidate.record = record;
        target.candidate.timestamp_quality = timestamp_quality;
        target.owner = owner;
        target.file_index = file_index;
        target.size += source_size;
        self.groups[b].parent = a;
        a
    }

    fn append(&mut self, candidate: Candidate, file_index: usize) {
        let index = self.groups.len();
        let keys = candidate.keys.clone();
        self.groups.push(Group {
            candidate,
            parent: index,
            size: 1,
            owner: index,
            file_index,
        });
        for key in keys {
            if let Some(existing) = self.identities.get(&key).copied() {
                self.merge(index, existing);
            }
            self.identities.insert(key, index);
        }
    }
}

/// `readAntigravityUsage`: reads every store under the roots and merges aliases before the
/// date filter; each merged record stays with the store that first produced it.
pub fn read_antigravity_usage(roots: &[PathBuf], since_ms: f64) -> AntigravityUsage {
    let mut files: Vec<AntigravityFile> = Vec::new();
    let mut errors: Vec<PathBuf> = Vec::new();
    let mut merger = Merger {
        groups: Vec::new(),
        identities: HashMap::new(),
    };
    let mut visited: HashSet<PathBuf> = HashSet::new();

    fn walk(directory: &Path, root: &Path, files: &mut Vec<AntigravityFile>, errors: &mut Vec<PathBuf>, merger: &mut Merger, visited: &mut HashSet<PathBuf>) {
        let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(directory) {
            Ok(entries) => entries.filter_map(Result::ok).collect(),
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    errors.push(directory.to_path_buf());
                }
                return;
            }
        };
        entries.sort_by_key(std::fs::DirEntry::file_name);
        entries.sort_by(|a, b| locale_compare(&a.file_name().to_string_lossy(), &b.file_name().to_string_lossy()));
        for entry in entries {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&path, root, files, errors, merger, visited);
            } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(".db") {
                let result = (|| -> Result<(), ()> {
                    let canonical = std::fs::canonicalize(&path).map_err(|_| ())?;
                    if !visited.insert(canonical) {
                        return Ok(());
                    }
                    let metadata = std::fs::metadata(&path).map_err(|_| ())?;
                    let candidates = read_database(&path, mtime_ms(&metadata)).map_err(|_| ())?;
                    let file_index = files.len();
                    files.push(AntigravityFile {
                        root: root.to_path_buf(),
                        path: path.clone(),
                        records: Vec::new(),
                    });
                    for candidate in candidates {
                        merger.append(candidate, file_index);
                    }
                    Ok(())
                })();
                if result.is_err() {
                    errors.push(path);
                }
            }
        }
    }

    for root in roots {
        walk(root, root, &mut files, &mut errors, &mut merger, &mut visited);
    }
    for index in 0..merger.groups.len() {
        let group = &merger.groups[index];
        if group.parent == index && group.candidate.record.timestamp_ms >= since_ms {
            files[group.file_index].records.push(group.candidate.record.clone());
        }
    }
    AntigravityUsage { files, errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_names() {
        assert_eq!(model_name("Gemini 3 Pro", 0.0), "gemini-3-pro");
        assert_eq!(model_name("Claude Opus 4.6", 0.0), "claude-opus-4-6");
        assert_eq!(model_name("Claude 4.5 Sonnet (Thinking)", 0.0), "claude-sonnet-4-5");
        assert_eq!(model_name("", 246.0), "gemini-2.5-pro");
        assert_eq!(model_name("", 9999.0), "antigravity-model-9999");
        assert_eq!(model_name("", 0.0), "");
    }
}
