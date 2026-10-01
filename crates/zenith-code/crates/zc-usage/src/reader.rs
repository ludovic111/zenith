//! Raw filesystem access for transcript scanning (`usageTranscriptReader.ts`).
//!
//! A transcript is read in large chunks and split on newlines in place; only lines that can
//! carry usage (a cheap substring gate, as in TS) are parsed, and only the fields the
//! parsers need are materialized ([`crate::json`]). A parse reports the byte position it
//! stopped at, fingerprinted by the bytes just before it, so a later scan of a grown file
//! parses only the appended bytes.
//!
//! Where TS switches to a streaming projection for lines above 8 MiB, this keeps the whole
//! line in memory and projects it the same way: the results are the same, the peak memory is
//! the longest line.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use memchr::memmem;
use zc_contracts::UsageProviderKind;

use crate::json;
use crate::records::{self, CodexScanState, UsageRecord};

/// A transcript found by [`list_transcript_files`].
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptFile {
    pub path: PathBuf,
    pub size: u64,
    /// `stats.mtimeMs`, computed like Node (`sec * 1e3 + nsec / 1e6`) so cached values compare.
    pub mtime_ms: f64,
}

/// Where a parse stopped, with enough state to continue (`TranscriptParsePosition`).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsePosition {
    /// Byte offset just past the last newline-terminated line consumed.
    pub resume_offset: u64,
    /// Length of the fingerprinted window ending at `resume_offset`.
    pub guard_length: u64,
    /// FNV-1a of that window (a double, as the cache file may hold any number).
    pub guard_hash: f64,
    /// Codex reducer state at `resume_offset`; `None` for stateless providers.
    pub codex_state: Option<CodexScanState>,
}

/// `TranscriptParseResult`.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseResult {
    /// Records of newline-terminated lines from the parse start.
    pub records: Vec<UsageRecord>,
    /// Records of a trailing line the writer has not terminated yet (re-read next time).
    pub tail_records: Vec<UsageRecord>,
    pub position: ParsePosition,
    /// Whether the parse continued from the given position rather than byte 0.
    pub resumed: bool,
}

/// 64 bytes of JSONL tail is ample to tell a replaced file from a grown one.
pub const GUARD_LENGTH: u64 = 64;
const CHUNK_SIZE: usize = 1024 * 1024;

/// `stats.mtimeMs`.
pub fn mtime_ms(metadata: &fs::Metadata) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let value = metadata.mtime() as f64 * 1e3 + metadata.mtime_nsec() as f64 / 1e6;
    value
}

/// Directory entries sorted by name bytes, like libuv's `scandir` (what `fs.readdir` returns).
pub fn read_dir_sorted(dir: &Path) -> std::io::Result<Vec<fs::DirEntry>> {
    let mut entries: Vec<fs::DirEntry> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

/// `.jsonl` files under `root` (or only `file_name`) modified at or after `since_ms`, in walk
/// order. Errors on entries are skipped: a partial listing beats a failed page.
pub fn list_transcript_files(root: &Path, since_ms: f64, file_name: Option<&str>) -> Vec<TranscriptFile> {
    let mut found = Vec::new();
    walk(root, since_ms, file_name, &mut found);
    found
}

fn walk(dir: &Path, since_ms: f64, file_name: Option<&str>, found: &mut Vec<TranscriptFile>) {
    let Ok(entries) = read_dir_sorted(dir) else {
        return;
    };
    for entry in entries {
        let child = entry.path();
        // Dirent types do not follow symlinks.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            walk(&child, since_ms, file_name, found);
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        match file_name {
            Some(wanted) if name != wanted => continue,
            None if !name.ends_with(".jsonl") => continue,
            _ => {}
        }
        if let Ok(metadata) = fs::metadata(&child) {
            let mtime = mtime_ms(&metadata);
            if mtime >= since_ms {
                found.push(TranscriptFile {
                    path: child,
                    size: metadata.len(),
                    mtime_ms: mtime,
                });
            }
        }
    }
}

/// Filesystem identity of a directory, `device:inode`, or `""` when it cannot be stat'd.
pub fn read_directory_volume_id(path: &Path) -> String {
    match fs::metadata(path) {
        #[allow(clippy::cast_precision_loss)]
        Ok(metadata) => format!(
            "{}:{}",
            json::number_to_string(metadata.dev() as f64),
            json::number_to_string(metadata.ino() as f64)
        ),
        Err(_) => String::new(),
    }
}

/// FNV-1a (32-bit).
pub fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn guard_matches(file: &File, position: &ParsePosition) -> bool {
    if position.guard_length == 0 || position.guard_length > GUARD_LENGTH || position.guard_length > position.resume_offset {
        return false;
    }
    #[allow(clippy::cast_possible_truncation)]
    let mut window = vec![0u8; position.guard_length as usize];
    match file.read_exact_at(&mut window, position.resume_offset - position.guard_length) {
        Ok(()) => f64::from(fnv1a(&window)) == position.guard_hash,
        Err(_) => false,
    }
}

/// The substring gates applied before parsing (`mightCarryUsage` and the Codex reducer lines).
struct Gates {
    usage: memmem::Finder<'static>,
    turn_completed: memmem::Finder<'static>,
    token_count: memmem::Finder<'static>,
    turn_context: memmem::Finder<'static>,
    session_meta: memmem::Finder<'static>,
}

impl Gates {
    fn new() -> Self {
        Self {
            usage: memmem::Finder::new(b"\"usage\""),
            turn_completed: memmem::Finder::new(b"\"turn_completed\""),
            token_count: memmem::Finder::new(b"\"token_count\""),
            turn_context: memmem::Finder::new(b"\"turn_context\""),
            session_meta: memmem::Finder::new(b"\"session_meta\""),
        }
    }
}

fn parse_line(gates: &Gates, provider: UsageProviderKind, line: &[u8], state: &mut CodexScanState, out: &mut Vec<UsageRecord>) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    match provider {
        UsageProviderKind::Codex => {
            if gates.token_count.find(line).is_none() && gates.turn_context.find(line).is_none() && gates.session_meta.find(line).is_none() {
                return;
            }
            if let Some(record) = records::parse_codex_line(line, state) {
                out.push(record);
            }
        }
        UsageProviderKind::Grok => {
            if gates.turn_completed.find(line).is_some() {
                out.extend(records::parse_grok_line(line));
            }
        }
        _ => {
            if gates.usage.find(line).is_some() {
                if let Some(record) = records::parse_claude_line(line) {
                    out.push(record);
                }
            }
        }
    }
}

/// `readTranscriptRecords`: the usage records of one transcript, or `None` when the file
/// could not be read (a transient failure must not be cached as an empty transcript).
///
/// With `resume_from`, parsing continues there when its guard bytes still match; otherwise
/// the whole file is parsed again and `resumed` is false.
pub fn read_transcript_records(path: &Path, provider: UsageProviderKind, resume_from: Option<&ParsePosition>) -> Option<ParseResult> {
    let mut file = File::open(path).ok()?;
    read_open_file(&mut file, provider, resume_from).ok()
}

fn read_open_file(file: &mut File, provider: UsageProviderKind, resume_from: Option<&ParsePosition>) -> std::io::Result<ParseResult> {
    let gates = Gates::new();
    let mut state = CodexScanState::default();
    let mut resumed = false;
    let mut start = 0u64;
    if let Some(position) = resume_from {
        if position.resume_offset > 0 && (provider != UsageProviderKind::Codex || position.codex_state.is_some()) && guard_matches(file, position) {
            if let Some(codex_state) = &position.codex_state {
                state = codex_state.clone();
            }
            start = position.resume_offset;
            resumed = true;
        }
    }
    file.seek(SeekFrom::Start(start))?;

    let mut records = Vec::new();
    let mut resume_offset = start;
    let mut scan_offset = start;
    let mut pending: Vec<u8> = Vec::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let chunk = &buffer[..read];
        let mut line_start = 0;
        while line_start < read {
            let Some(relative) = memchr::memchr(b'\n', &chunk[line_start..]) else {
                pending.extend_from_slice(&chunk[line_start..]);
                break;
            };
            let newline = line_start + relative;
            if pending.is_empty() {
                parse_line(&gates, provider, &chunk[line_start..newline], &mut state, &mut records);
            } else {
                pending.extend_from_slice(&chunk[line_start..newline]);
                parse_line(&gates, provider, &pending, &mut state, &mut records);
                pending.clear();
            }
            line_start = newline + 1;
            resume_offset = scan_offset + line_start as u64;
        }
        scan_offset += read as u64;
    }

    // The unterminated tail parses against a copy of the state: it is replayed next time.
    let mut tail_records = Vec::new();
    if !pending.is_empty() {
        let mut tail_state = state.clone();
        parse_line(&gates, provider, &pending, &mut tail_state, &mut tail_records);
    }

    let guard_length = GUARD_LENGTH.min(resume_offset);
    let mut guard_hash = 0u32;
    if guard_length > 0 {
        #[allow(clippy::cast_possible_truncation)]
        let mut window = vec![0u8; guard_length as usize];
        file.read_exact_at(&mut window, resume_offset - guard_length)?;
        guard_hash = fnv1a(&window);
    }
    Ok(ParseResult {
        records,
        tail_records,
        position: ParsePosition {
            resume_offset,
            guard_length,
            guard_hash: f64::from(guard_hash),
            codex_state: (provider == UsageProviderKind::Codex).then_some(state),
        },
        resumed,
    })
}

#[cfg(test)]
mod tests;
