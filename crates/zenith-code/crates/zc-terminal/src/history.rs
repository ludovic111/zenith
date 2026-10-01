//! `BoundedTerminalHistory` of `terminal/Manager.ts`: the scrollback kept per terminal.
//!
//! The history keeps the longest suffix of everything appended that has at most `max_lines`
//! lines and at most `max_bytes` UTF-8 bytes, cut at a character boundary. A line is counted
//! per `\n`, plus one for an unterminated last line. Text is stored in chunks of at most
//! 16 KiB so trimming the front never moves the whole history, and [`value`] is cached.
//!
//! A zero limit keeps nothing, except that with `max_bytes > 0` an append ending in `\n`
//! leaves `"\n"` (the TS behaviour). Lone UTF-16 surrogates, which the TS class joins across
//! appends, cannot occur in Rust strings (PTY output is decoded before it gets here).
//!
//! [`value`]: BoundedTerminalHistory::value

use std::collections::VecDeque;

/// `DEFAULT_HISTORY_LINE_LIMIT`.
pub const DEFAULT_HISTORY_LINE_LIMIT: usize = 5_000;
/// `DEFAULT_HISTORY_BYTE_LIMIT` (8 MiB).
pub const DEFAULT_HISTORY_BYTE_LIMIT: usize = 8 * 1024 * 1024;
/// `MAX_HISTORY_CHUNK_LENGTH`, in bytes here (16 Ki UTF-16 code units in TS).
const MAX_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone)]
struct Chunk {
    data: String,
    line_breaks: usize,
}

/// See the module docs.
#[derive(Debug, Clone)]
pub struct BoundedTerminalHistory {
    max_lines: usize,
    max_bytes: usize,
    chunks: VecDeque<Chunk>,
    byte_length: usize,
    line_breaks: usize,
    cached: Option<String>,
}

fn count_line_breaks(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count()
}

impl BoundedTerminalHistory {
    /// `new BoundedTerminalHistory(maxLines, initial, maxBytes)`.
    pub fn new(max_lines: usize, initial: &str, max_bytes: usize) -> Self {
        let mut history = Self {
            max_lines,
            max_bytes,
            chunks: VecDeque::new(),
            byte_length: 0,
            line_breaks: 0,
            cached: Some(String::new()),
        };
        history.append(initial);
        history
    }

    /// Size in UTF-8 bytes.
    pub fn byte_length(&self) -> usize {
        self.byte_length
    }

    pub fn is_empty(&self) -> bool {
        self.byte_length == 0
    }

    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.cached = None;
        if self.max_bytes == 0 || self.max_lines == 0 {
            self.clear();
            // The zero-line limit keeps a trailing newline.
            if self.max_bytes > 0 && text.ends_with('\n') {
                self.append_chunk("\n");
            }
            return;
        }
        let mut offset = 0;
        while offset < text.len() {
            let mut end = (offset + MAX_CHUNK_BYTES).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            self.append_chunk(&text[offset..end]);
            self.trim();
            offset = end;
        }
    }

    fn append_chunk(&mut self, data: &str) {
        let line_breaks = count_line_breaks(data);
        match self.chunks.back_mut() {
            Some(previous) if previous.data.len() + data.len() <= MAX_CHUNK_BYTES => {
                previous.data.push_str(data);
                previous.line_breaks += line_breaks;
            }
            _ => self.chunks.push_back(Chunk {
                data: data.to_owned(),
                line_breaks,
            }),
        }
        self.byte_length += data.len();
        self.line_breaks += line_breaks;
        self.cached = None;
    }

    fn ends_with_newline(&self) -> bool {
        self.chunks.back().is_some_and(|chunk| chunk.data.ends_with('\n'))
    }

    fn discard_front(&mut self) {
        if let Some(first) = self.chunks.pop_front() {
            self.byte_length -= first.data.len();
            self.line_breaks -= first.line_breaks;
        }
    }

    /// Drops the first `offset` bytes (a char boundary) of the first chunk.
    fn trim_front(&mut self, offset: usize) {
        let Some(first) = self.chunks.front_mut() else {
            return;
        };
        if offset >= first.data.len() {
            self.discard_front();
            return;
        }
        let line_breaks = count_line_breaks(&first.data[..offset]);
        first.data.drain(..offset);
        first.line_breaks -= line_breaks;
        self.byte_length -= offset;
        self.line_breaks -= line_breaks;
    }

    fn trim(&mut self) {
        let lines = self.line_breaks + usize::from(!self.ends_with_newline());
        let mut lines_to_drop = lines.saturating_sub(self.max_lines);
        while lines_to_drop > 0 {
            let Some(first) = self.chunks.front() else {
                break;
            };
            if first.line_breaks < lines_to_drop {
                lines_to_drop -= first.line_breaks;
                self.discard_front();
                continue;
            }
            let offset = first
                .data
                .match_indices('\n')
                .nth(lines_to_drop - 1)
                .map(|(index, _)| index + 1)
                .unwrap_or(first.data.len());
            self.trim_front(offset);
            lines_to_drop = 0;
        }

        while self.byte_length > self.max_bytes {
            let Some(first) = self.chunks.front() else {
                break;
            };
            let bytes_to_drop = self.byte_length - self.max_bytes;
            if first.data.len() <= bytes_to_drop {
                self.discard_front();
                continue;
            }
            let mut offset = bytes_to_drop;
            while !first.data.is_char_boundary(offset) {
                offset += 1;
            }
            self.trim_front(offset);
        }
    }

    pub fn clear(&mut self) {
        self.chunks.clear();
        self.byte_length = 0;
        self.line_breaks = 0;
        self.cached = Some(String::new());
    }

    /// The retained text.
    pub fn value(&mut self) -> &str {
        if self.cached.is_none() {
            let mut value = String::with_capacity(self.byte_length);
            for chunk in &self.chunks {
                value.push_str(&chunk.data);
            }
            self.cached = Some(value);
        }
        self.cached.as_deref().unwrap_or_default()
    }

    /// [`value`](Self::value), owned.
    pub fn to_value(&mut self) -> String {
        self.value().to_owned()
    }
}

/// The reference model of `Manager.test.ts` (`retainedHistory`): the line policy, then the
/// longest code-point-aligned byte tail.
pub fn retained_history(text: &str, max_lines: usize, max_bytes: usize) -> String {
    let terminated = text.ends_with('\n');
    let mut lines: Vec<&str> = text.split('\n').collect();
    if terminated {
        lines.pop();
    }
    let start = lines.len().saturating_sub(max_lines);
    let retained = lines[start..].join("\n");
    let capped = if terminated { format!("{retained}\n") } else { retained };
    if capped.len() <= max_bytes {
        return capped;
    }
    let mut offset = capped.len() - max_bytes;
    while !capped.is_char_boundary(offset) {
        offset += 1;
    }
    capped[offset..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // "preserves line and byte limits across arbitrary chunks, Unicode, ANSI sequences, and
    // clear" (without the lone-surrogate fragments, which Rust strings cannot hold).
    #[test]
    fn preserves_limits_across_arbitrary_chunks() {
        let mut seed: u32 = 0x2026_0904;
        let fragments = [
            "",
            "a",
            "\n",
            "\n\n",
            "\r",
            "\r\n",
            "café",
            "名",
            "🚀",
            "\u{1b}[31m",
            "\u{1b}[0m",
            "\u{1b}]8;;url\u{7}",
        ];
        let mut next = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            fragments[(seed as usize) % fragments.len()]
        };
        for max_bytes in [0, 3, 8, 64, usize::MAX] {
            for max_lines in [0, 1, 3, 5, 5_000] {
                let initial = "before\ninitial\n";
                let mut history = BoundedTerminalHistory::new(max_lines, initial, max_bytes);
                let mut expected = retained_history(initial, max_lines, max_bytes);
                assert_eq!(history.value(), expected);
                for step in 0..300 {
                    if step % 73 == 0 {
                        history.clear();
                        expected.clear();
                        assert_eq!(history.value(), expected);
                    }
                    let chunk = format!("{}{}", next(), next());
                    history.append(&chunk);
                    expected = retained_history(&(expected + &chunk), max_lines, max_bytes);
                    assert_eq!(history.value(), expected, "lines={max_lines} bytes={max_bytes}");
                }
            }
        }
    }

    #[test]
    fn bounds_long_partial_lines() {
        let max_bytes = 65_539;
        let mut expected = String::new();
        let mut history = BoundedTerminalHistory::new(5_000, "", max_bytes);
        for text in [
            format!("{}😀{}", "a".repeat(16_383), "b".repeat(70_000)),
            format!("\r{}", "c".repeat(70_000)),
            "d".repeat(100),
            format!("\u{feff}{}", "名".repeat(30_000)),
        ] {
            history.append(&text);
            expected = retained_history(&(expected + &text), 5_000, max_bytes);
            assert_eq!(history.value(), expected);
            assert!(history.value().len() <= max_bytes);
        }
    }

    #[test]
    fn preserves_retained_lines_as_storage_is_compacted() {
        for max_lines in [3, 5_000] {
            let mut expected = String::new();
            let mut history = BoundedTerminalHistory::new(max_lines, "", DEFAULT_HISTORY_BYTE_LIMIT);
            for batch in 0..40 {
                let chunk: String = (0..300).map(|line| format!("{batch}:{line}\n")).collect();
                history.append(&chunk);
                expected = retained_history(&(expected + &chunk), max_lines, usize::MAX);
                assert_eq!(history.value(), expected);
            }
        }
    }
}
