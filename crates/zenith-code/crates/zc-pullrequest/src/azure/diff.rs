//! `pullRequest/azureDevOpsDiff.ts`: the unified patch Azure DevOps does not serve, built here out
//! of both sides of every changed file.
//!
//! The hunks come from a port of jsdiff 8's `structuredPatch` (its line tokenizer, its Myers
//! variant with the diagonal pruning and the `maxEditLength` / `timeout` bail-outs, and its hunk
//! assembly), so the patch is byte for byte what the TS server writes. A generic Myers diff (the
//! `similar` crate) finds an equally short edit script but is free to break ties another way,
//! which moves hunks (flipping one comparison of jsdiff's tie-break changes 94 of the golden
//! cases); the golden test (`tests/azure_golden.rs`) holds the two implementations together.

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::azure::json::{AzureDevOpsChangeEntry, AzureDevOpsChangeKind};
use crate::azure::util::quote_git_patch_path;

/// How far a diff read got, and which push it was reading: a push landing mid-read would
/// renumber the list under a cursor that did not also pin the iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AzureDevOpsDiffCursor {
    pub iteration_id: i64,
    pub file_index: i64,
}

const CURSOR_SEPARATOR: char = ':';

/// `formatAzureDevOpsDiffCursor`.
pub fn format_azure_devops_diff_cursor(cursor: AzureDevOpsDiffCursor) -> String {
    format!("{}{CURSOR_SEPARATOR}{}", cursor.iteration_id, cursor.file_index)
}

/// `Number(text)` for a string of plain decimal digits, `None` past a safe integer.
fn decimal(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    (value <= 9_007_199_254_740_991.0).then_some(value as i64)
}

/// `parseAzureDevOpsDiffCursor`: `None` for anything this did not write, which starts the read
/// from the top rather than failing it. Each half is plain decimal only.
pub fn parse_azure_devops_diff_cursor(raw: Option<&str>) -> Option<AzureDevOpsDiffCursor> {
    let mut parts = raw?.split(CURSOR_SEPARATOR);
    let iteration = parts.next()?;
    let file = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let iteration_id = decimal(iteration)?;
    let file_index = decimal(file)?;
    (iteration_id > 0).then_some(AzureDevOpsDiffCursor { iteration_id, file_index })
}

/// The two texts of one changed file, empty on whichever side the change does not have.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AzureDevOpsFileTexts {
    pub old_contents: String,
    pub new_contents: String,
    /// Azure's own flag for a file it hands back base64-encoded instead of as text.
    pub binary: bool,
}

/// One file's section of the patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AzureDevOpsFilePatch {
    pub section: String,
    /// The file changed but its hunks are not in the section.
    pub truncated: bool,
    /// The diff was given up on partway, having spent the whole edit budget, so a caller reading
    /// a run of files stops here.
    pub abandoned: bool,
    /// Lines added or removed: the edit distance the diff searched out, which is what a slice's
    /// budget is spent in.
    pub edits: usize,
}

/// Beyond this a file is shown as changed without its hunks.
const MAX_FILE_BYTES: usize = 512 * 1024;

/// Git's own default context.
const PATCH_CONTEXT_LINES: usize = 3;

/// How far apart one file's two sides may be before it is listed without its hunks. The line
/// diff costs about the square of the edit distance; bounded in edits rather than time so a
/// change slices the same way on every machine.
pub const MAX_FILE_DIFF_EDITS: usize = 2_000;

/// A backstop for a machine slower than the edit ceiling was tuned for.
const MAX_FILE_DIFF_MILLIS: u64 = 2_000;

/// How much diff work one slice does before the rest is left for the next request.
pub const MAX_DIFF_SLICE_EDITS: usize = 6_000;

/// How much patch one slice carries before the rest is left for the next request.
pub const MAX_DIFF_SLICE_BYTES: usize = 256 * 1024;

/// How many files one slice carries however little each one weighs.
pub const MAX_DIFF_SLICE_FILES: usize = 300;

/// Git's own note for a side whose last line has no newline after it.
const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file";

/// A text's lines, without the empty one that a trailing newline leaves behind a split.
fn content_lines(contents: &str) -> Vec<&str> {
    if contents.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = contents.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// A NUL byte is git's own test for a binary file, and it survives Azure's JSON envelope.
fn is_binary(contents: &str) -> bool {
    contents.contains('\0')
}

/// What a file costs on the wire: its UTF-8 bytes.
pub fn byte_length(contents: &str) -> usize {
    contents.len()
}

/// Git points an empty range at the line before it and writes a single line as its number alone.
fn hunk_range(start: usize, lines: usize) -> String {
    if lines == 0 {
        return format!("{},0", start as i64 - 1);
    }
    if lines == 1 {
        start.to_string()
    } else {
        format!("{start},{lines}")
    }
}

/// The `diff --git` preamble. Azure reports no file mode, so the ordinary one stands in; names
/// are quoted the way git quotes them, side letter inside the quoting.
fn patch_header(change: &AzureDevOpsChangeEntry) -> String {
    let old_side = quote_git_patch_path(&format!("a/{}", change.old_path));
    let new_side = quote_git_patch_path(&format!("b/{}", change.path));
    let mut lines = vec![format!("diff --git {old_side} {new_side}")];
    match change.change_kind {
        AzureDevOpsChangeKind::New => lines.push("new file mode 100644".into()),
        AzureDevOpsChangeKind::Deleted => lines.push("deleted file mode 100644".into()),
        AzureDevOpsChangeKind::RenamePure | AzureDevOpsChangeKind::RenameChanged => {
            lines.push(format!("rename from {}", quote_git_patch_path(&change.old_path)));
            lines.push(format!("rename to {}", quote_git_patch_path(&change.path)));
        }
        AzureDevOpsChangeKind::Change => {}
    }
    let new_file = change.change_kind == AzureDevOpsChangeKind::New;
    let deleted = change.change_kind == AzureDevOpsChangeKind::Deleted;
    lines.push(format!("--- {}", if new_file { "/dev/null" } else { old_side.as_str() }));
    lines.push(format!("+++ {}", if deleted { "/dev/null" } else { new_side.as_str() }));
    lines.join("\n")
}

/// A file written out as wholly replaced, in one hunk, which needs no edit-distance search.
fn replacement_section(header: &str, texts: &AzureDevOpsFileTexts) -> String {
    let old_lines = content_lines(&texts.old_contents);
    let new_lines = content_lines(&texts.new_contents);
    let no_newline = |contents: &str, lines: &[&str]| !lines.is_empty() && !contents.ends_with('\n');
    let mut out = vec![
        header.to_owned(),
        format!("@@ -{} +{} @@", hunk_range(1, old_lines.len()), hunk_range(1, new_lines.len())),
    ];
    out.extend(old_lines.iter().map(|line| format!("-{line}")));
    if no_newline(&texts.old_contents, &old_lines) {
        out.push(NO_NEWLINE_MARKER.into());
    }
    out.extend(new_lines.iter().map(|line| format!("+{line}")));
    if no_newline(&texts.new_contents, &new_lines) {
        out.push(NO_NEWLINE_MARKER.into());
    }
    out.push(String::new());
    out.join("\n")
}

/// `azureDevOpsFilePatch`: one file's section of a unified patch.
pub fn azure_devops_file_patch(change: &AzureDevOpsChangeEntry, texts: &AzureDevOpsFileTexts) -> AzureDevOpsFilePatch {
    let header = patch_header(change);
    let old_contents = texts.old_contents.as_str();
    let new_contents = texts.new_contents.as_str();
    let header_only = |truncated: bool, abandoned: bool, edits: usize| AzureDevOpsFilePatch {
        section: format!("{header}\n"),
        truncated,
        abandoned,
        edits,
    };

    if texts.binary || is_binary(old_contents) || is_binary(new_contents) {
        // Git's own wording for a file it will not spell out.
        let old_side = quote_git_patch_path(&format!("a/{}", change.old_path));
        let new_side = quote_git_patch_path(&format!("b/{}", change.path));
        return AzureDevOpsFilePatch {
            section: format!("{header}\nBinary files {old_side} and {new_side} differ\n"),
            truncated: true,
            abandoned: false,
            edits: 0,
        };
    }
    if byte_length(old_contents) > MAX_FILE_BYTES || byte_length(new_contents) > MAX_FILE_BYTES {
        return header_only(true, false, 0);
    }

    // A creation or deletion has nothing on one side, so both sides in full is already the
    // minimal patch.
    let created = old_contents.is_empty() && !new_contents.is_empty();
    let deleted = new_contents.is_empty() && !old_contents.is_empty();
    if created || deleted {
        let contents = if created { new_contents } else { old_contents };
        let lines = content_lines(contents).len();
        // A marker on every line can put a side that just fits the ceiling over it; checked as
        // bytes plus marker count before building anything.
        if byte_length(contents) + lines > MAX_FILE_BYTES {
            return header_only(true, false, lines);
        }
        let section = replacement_section(&header, texts);
        if byte_length(&section) > MAX_FILE_BYTES {
            return header_only(true, false, lines);
        }
        return AzureDevOpsFilePatch {
            section,
            truncated: false,
            abandoned: false,
            edits: lines,
        };
    }

    let options = PatchOptions {
        context: PATCH_CONTEXT_LINES,
        max_edit_length: Some(MAX_FILE_DIFF_EDITS),
        timeout: Some(Duration::from_millis(MAX_FILE_DIFF_MILLIS)),
    };
    // Hitting the edit ceiling lists the file without hunks rather than as a full replacement,
    // which would bury a small change in a long file under a wall of text.
    let Some(hunks) = structured_patch(old_contents, new_contents, &options) else {
        return header_only(true, true, MAX_FILE_DIFF_EDITS);
    };

    let mut edits = 0;
    let rendered: Vec<String> = hunks
        .iter()
        .map(|hunk| {
            edits += hunk.lines.iter().filter(|line| line.starts_with('+') || line.starts_with('-')).count();
            let mut lines = vec![format!(
                "@@ -{} +{} @@",
                hunk_range(hunk.old_start, hunk.old_lines),
                hunk_range(hunk.new_start, hunk.new_lines)
            )];
            lines.extend(hunk.lines.iter().cloned());
            lines.join("\n")
        })
        .collect();
    // A pure rename has no hunks but is still listed.
    let section = if rendered.is_empty() {
        format!("{header}\n")
    } else {
        format!("{header}\n{}\n", rendered.join("\n"))
    };
    // A handful of very long lines plus context can exceed the size ceiling well inside the edit
    // ceiling.
    if byte_length(&section) > MAX_FILE_BYTES {
        return header_only(true, false, edits);
    }
    AzureDevOpsFilePatch {
        section,
        truncated: false,
        abandoned: false,
        edits,
    }
}

/// `azureDevOpsUnreadableFilePatch`: a file listed without its hunks, for when the host would not
/// hand one of its two sides over.
pub fn azure_devops_unreadable_file_patch(change: &AzureDevOpsChangeEntry) -> AzureDevOpsFilePatch {
    AzureDevOpsFilePatch {
        section: format!("{}\n", patch_header(change)),
        truncated: true,
        abandoned: false,
        edits: 0,
    }
}

// ---------------------------------------------------------------------------------------------
// jsdiff 8 `structuredPatch` (`diff/base.js`, `diff/line.js`, `patch/create.js`).
// ---------------------------------------------------------------------------------------------

/// The options of `structuredPatch` this module uses.
#[derive(Debug, Clone, Copy)]
pub struct PatchOptions {
    pub context: usize,
    pub max_edit_length: Option<usize>,
    pub timeout: Option<Duration>,
}

/// One hunk of `structuredPatch` (starts are 1-based, not yet adjusted for empty ranges).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredHunk {
    pub old_start: usize,
    pub old_lines: usize,
    pub new_start: usize,
    pub new_lines: usize,
    pub lines: Vec<String>,
}

/// One change object of jsdiff (`{count, added, removed}`), as a persistent linked list.
struct Component {
    count: usize,
    added: bool,
    removed: bool,
    previous: Option<Rc<Component>>,
}

impl Drop for Component {
    /// Unlinks the chain iteratively: a long path would otherwise drop recursively.
    fn drop(&mut self) {
        let mut next = self.previous.take();
        while let Some(component) = next {
            match Rc::try_unwrap(component) {
                Ok(mut owned) => next = owned.previous.take(),
                Err(_) => break,
            }
        }
    }
}

#[derive(Clone)]
struct Path {
    old_pos: i64,
    last: Option<Rc<Component>>,
}

/// `addToPath`.
fn add_to_path(path: &Path, added: bool, removed: bool, old_pos_inc: i64) -> Path {
    let last = match &path.last {
        Some(last) if last.added == added && last.removed == removed => Component {
            count: last.count + 1,
            added,
            removed,
            previous: last.previous.clone(),
        },
        last => Component {
            count: 1,
            added,
            removed,
            previous: last.clone(),
        },
    };
    Path {
        old_pos: path.old_pos + old_pos_inc,
        last: Some(Rc::new(last)),
    }
}

/// `extractCommon`: follows the diagonal while both sides agree; returns the new position.
fn extract_common(path: &mut Path, new_tokens: &[u32], old_tokens: &[u32], diagonal: i64) -> i64 {
    let (new_len, old_len) = (new_tokens.len() as i64, old_tokens.len() as i64);
    let mut old_pos = path.old_pos;
    let mut new_pos = old_pos - diagonal;
    let mut common = 0;
    while new_pos + 1 < new_len && old_pos + 1 < old_len && old_tokens[(old_pos + 1) as usize] == new_tokens[(new_pos + 1) as usize] {
        new_pos += 1;
        old_pos += 1;
        common += 1;
    }
    if common > 0 {
        path.last = Some(Rc::new(Component {
            count: common,
            added: false,
            removed: false,
            previous: path.last.take(),
        }));
    }
    path.old_pos = old_pos;
    new_pos
}

/// `buildValues`, keeping each component's token range rather than joining it.
fn build_values(last: Option<Rc<Component>>) -> Vec<(usize, bool, bool)> {
    let mut components = Vec::new();
    let mut next = last;
    while let Some(component) = next {
        components.push((component.count, component.added, component.removed));
        next = component.previous.clone();
    }
    components.reverse();
    components
}

/// `Diff.diffWithOptionsObj` for lines: the change objects as `(count, added, removed)`, `None`
/// when the edit length or the time ran out.
fn diff_tokens(old_tokens: &[u32], new_tokens: &[u32], options: &PatchOptions) -> Option<Vec<(usize, bool, bool)>> {
    let (new_len, old_len) = (new_tokens.len() as i64, old_tokens.len() as i64);
    let mut edit_length: i64 = 1;
    let mut max_edit_length = new_len + old_len;
    if let Some(limit) = options.max_edit_length {
        max_edit_length = max_edit_length.min(limit as i64);
    }
    let abort_after = options.timeout.and_then(|timeout| Instant::now().checked_add(timeout));
    let offset = max_edit_length + 2;
    let mut best_path: Vec<Option<Path>> = vec![None; (2 * offset + 1) as usize];
    let slot = |diagonal: i64| (diagonal + offset) as usize;

    let mut seed = Path { old_pos: -1, last: None };
    let new_pos = extract_common(&mut seed, new_tokens, old_tokens, 0);
    if seed.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
        return Some(build_values(seed.last));
    }
    best_path[slot(0)] = Some(seed);

    let mut min_diagonal = i64::MIN;
    let mut max_diagonal = i64::MAX;
    while edit_length <= max_edit_length && abort_after.is_none_or(|deadline| Instant::now() <= deadline) {
        let mut diagonal = min_diagonal.max(-edit_length);
        while diagonal <= max_diagonal.min(edit_length) {
            let remove_path = best_path[slot(diagonal - 1)].take();
            let add_path = best_path[slot(diagonal + 1)].clone();
            let can_add = add_path.as_ref().is_some_and(|path| {
                let new_pos = path.old_pos - diagonal;
                0 <= new_pos && new_pos < new_len
            });
            let can_remove = remove_path.as_ref().is_some_and(|path| path.old_pos + 1 < old_len);
            if !can_add && !can_remove {
                best_path[slot(diagonal)] = None;
                diagonal += 2;
                continue;
            }
            // Branch from the prior path farthest from the origin in the old text.
            let use_add = match (&remove_path, &add_path) {
                (Some(remove), Some(add)) => !can_remove || (can_add && remove.old_pos < add.old_pos),
                _ => !can_remove,
            };
            let mut base = match (use_add, &remove_path, &add_path) {
                (true, _, Some(add)) => add_to_path(add, true, false, 0),
                (false, Some(remove), _) => add_to_path(remove, false, true, 1),
                _ => unreachable!("the path chosen to extend exists"),
            };
            let new_pos = extract_common(&mut base, new_tokens, old_tokens, diagonal);
            if base.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return Some(build_values(base.last));
            }
            if base.old_pos + 1 >= old_len {
                max_diagonal = max_diagonal.min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                min_diagonal = min_diagonal.max(diagonal + 1);
            }
            best_path[slot(diagonal)] = Some(base);
            diagonal += 2;
        }
        edit_length += 1;
    }
    None
}

/// jsdiff's line `tokenize` (no options): every line with its `\n` (or `\r\n`), the last one
/// without when the text does not end in a newline.
fn tokenize(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// `structuredPatch(…, options).hunks`, `None` where jsdiff answers `undefined` (the edit length
/// or the timeout ran out).
pub fn structured_patch(old_text: &str, new_text: &str, options: &PatchOptions) -> Option<Vec<StructuredHunk>> {
    let old_lines = tokenize(old_text);
    let new_lines = tokenize(new_text);
    let mut ids: HashMap<&str, u32> = HashMap::new();
    let mut old_tokens = Vec::with_capacity(old_lines.len());
    for line in &old_lines {
        let next = ids.len() as u32;
        old_tokens.push(*ids.entry(line).or_insert(next));
    }
    let mut new_tokens = Vec::with_capacity(new_lines.len());
    for line in &new_lines {
        let next = ids.len() as u32;
        new_tokens.push(*ids.entry(line).or_insert(next));
    }
    let components = diff_tokens(&old_tokens, &new_tokens, options)?;

    // The lines of every component, in order (`buildValues` + `splitLines`).
    let mut diff: Vec<(Vec<&str>, bool, bool)> = Vec::with_capacity(components.len() + 1);
    let (mut old_at, mut new_at) = (0usize, 0usize);
    for (count, added, removed) in components {
        if removed {
            diff.push((old_lines[old_at..old_at + count].to_vec(), false, true));
            old_at += count;
        } else {
            diff.push((new_lines[new_at..new_at + count].to_vec(), added, false));
            new_at += count;
            if !added {
                old_at += count;
            }
        }
    }
    // jsdiff appends an empty common value to make the cleanup easier.
    diff.push((Vec::new(), false, false));

    let context = options.context;
    let context_lines = |lines: &[&str]| -> Vec<String> { lines.iter().map(|line| format!(" {line}")).collect() };
    let mut hunks: Vec<StructuredHunk> = Vec::new();
    let (mut old_range_start, mut new_range_start) = (0usize, 0usize);
    let mut current_range: Vec<String> = Vec::new();
    let (mut old_line, mut new_line) = (1usize, 1usize);
    for i in 0..diff.len() {
        let (lines, added, removed) = (&diff[i].0, diff[i].1, diff[i].2);
        if added || removed {
            if old_range_start == 0 {
                old_range_start = old_line;
                new_range_start = new_line;
                if i > 0 {
                    let previous = &diff[i - 1].0;
                    current_range = if context > 0 {
                        context_lines(&previous[previous.len().saturating_sub(context)..])
                    } else {
                        Vec::new()
                    };
                    old_range_start -= current_range.len();
                    new_range_start -= current_range.len();
                }
            }
            let marker = if added { '+' } else { '-' };
            current_range.extend(lines.iter().map(|line| format!("{marker}{line}")));
            if added {
                new_line += lines.len();
            } else {
                old_line += lines.len();
            }
        } else {
            if old_range_start != 0 {
                if lines.len() <= context * 2 && i + 2 < diff.len() {
                    // Overlapping context: the hunk carries on.
                    current_range.extend(context_lines(lines));
                } else {
                    let context_size = lines.len().min(context);
                    current_range.extend(context_lines(&lines[..context_size]));
                    hunks.push(StructuredHunk {
                        old_start: old_range_start,
                        old_lines: old_line - old_range_start + context_size,
                        new_start: new_range_start,
                        new_lines: new_line - new_range_start + context_size,
                        lines: std::mem::take(&mut current_range),
                    });
                    old_range_start = 0;
                    new_range_start = 0;
                }
            }
            old_line += lines.len();
            new_line += lines.len();
        }
    }

    // Drop each line's trailing `\n`, and mark a line that had none.
    for hunk in &mut hunks {
        let mut lines = Vec::with_capacity(hunk.lines.len());
        for mut line in hunk.lines.drain(..) {
            if line.ends_with('\n') {
                line.pop();
                lines.push(line);
            } else {
                lines.push(line);
                lines.push(NO_NEWLINE_MARKER.to_owned());
            }
        }
        hunk.lines = lines;
    }
    Some(hunks)
}
