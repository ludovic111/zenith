//! The history sanitizer (`sanitizeTerminalHistoryChunk` of `terminal/Manager.ts`).
//!
//! Terminal output is stored for replay (the snapshot's `history`). Request/response traffic
//! must not be stored: replaying a query makes the client's terminal answer again, and the
//! shell then echoes the answer as junk at the prompt. Dropped before storage:
//!
//! - CSI `…n` (DSR), `…R` (CPR reply, body `[0-9;?]*`), `…c` (DA, body `[>0-9;?]*`),
//!   `…$p` / `…$y` (DECRQM / DECRPM, body `[0-9;?]*$`), `>…q` (XTVERSION), `?…u` (kitty
//!   keyboard query/reply);
//! - DCS `[01]?$q`, `[01]?$r`, `[01]?+q`, `[01]?+r` (DECRQSS, XTGETTCAP and their replies);
//! - OSC `10;`/`11;`/`12;` followed by `?` or `rgb:` (color queries and replies).
//!
//! Everything else (SGR, cursor moves, clears, titles, links, setters that share a final byte
//! with a query) is kept byte for byte. 7-bit (`ESC [`, `ESC ]`, `ESC P`, `ESC ^`, `ESC _`)
//! and 8-bit C1 forms (U+009B, U+009D, U+0090, U+009E, U+009F) are both recognised; strings
//! end at BEL, U+009C or `ESC \`. A sequence cut by a chunk boundary is carried over in
//! `pending` and completed by the next chunk.
//!
//! The TS version walks UTF-16 code units; every character it compares against is in the
//! BMP and the decoded PTY output never holds lone surrogates, so walking the UTF-8 bytes
//! of a Rust `str` (and only ever cutting at those ASCII/C1 boundaries) gives the same text.
//! The byte-exact fixtures in `fixtures/sanitizer.json` are generated from the TS function.

/// The result of [`sanitize_terminal_history_chunk`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SanitizedChunk {
    /// What goes into the history.
    pub visible_text: String,
    /// An unfinished control sequence, to prepend to the next chunk.
    pub pending_control_sequence: String,
}

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;

/// The C1 control at `index` (U+0080..=U+009F, encoded `C2 xx`), if any.
fn c1_at(bytes: &[u8], index: usize) -> Option<u8> {
    if bytes.get(index) == Some(&0xc2) {
        match bytes.get(index + 1) {
            Some(&next) if (0x80..=0x9f).contains(&next) => Some(next),
            _ => None,
        }
    } else {
        None
    }
}

fn is_csi_final_byte(byte: u8) -> bool {
    (0x40..=0x7e).contains(&byte)
}

fn all_bytes(body: &str, allowed: impl Fn(u8) -> bool) -> bool {
    body.bytes().all(allowed)
}

fn is_digit_semicolon_question(byte: u8) -> bool {
    byte.is_ascii_digit() || byte == b';' || byte == b'?'
}

fn should_strip_csi_sequence(body: &str, final_byte: u8) -> bool {
    match final_byte {
        b'n' => true,
        b'R' => all_bytes(body, is_digit_semicolon_question),
        b'c' => all_bytes(body, |b| b == b'>' || is_digit_semicolon_question(b)),
        // DECRQM mode queries (…$p) and DECRPM replies (…$y). The `$` guard keeps setters
        // like DECSTR (!p) and DECSCL ("p) intact.
        b'p' | b'y' => body.strip_suffix('$').is_some_and(|rest| all_bytes(rest, is_digit_semicolon_question)),
        // XTVERSION query (>q). DECSCUSR (space-intermediate q) stays.
        b'q' => body.strip_prefix('>').is_some_and(|rest| all_bytes(rest, |b| b.is_ascii_digit() || b == b';')),
        // Kitty keyboard protocol query/reply (?u). Restore-cursor (bare u) stays.
        b'u' => body.starts_with('?'),
        _ => false,
    }
}

/// `/^[01]?[$+][qr]/`: DECRQSS and XTGETTCAP queries and replies.
fn should_strip_dcs_sequence(content: &str) -> bool {
    let bytes = content.as_bytes();
    let start = usize::from(matches!(bytes.first(), Some(b'0' | b'1')));
    matches!(bytes.get(start), Some(b'$' | b'+')) && matches!(bytes.get(start + 1), Some(b'q' | b'r'))
}

/// `/^(10|11|12);(?:\?|rgb:)/`: color queries and replies.
fn should_strip_osc_sequence(content: &str) -> bool {
    let rest = ["10;", "11;", "12;"].iter().find_map(|prefix| content.strip_prefix(prefix));
    rest.is_some_and(|rest| rest.starts_with('?') || rest.starts_with("rgb:"))
}

fn strip_string_terminator(value: &str) -> &str {
    if let Some(stripped) = value.strip_suffix("\u{1b}\\") {
        return stripped;
    }
    value.strip_suffix('\u{7}').or_else(|| value.strip_suffix('\u{9c}')).unwrap_or(value)
}

/// The end (exclusive) of the string terminator at or after `start`: BEL, ST (U+009C) or
/// `ESC \`.
fn find_string_terminator_index(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            BEL => return Some(index + 1),
            ESC if bytes.get(index + 1) == Some(&b'\\') => return Some(index + 2),
            0xc2 if bytes.get(index + 1) == Some(&0x9c) => return Some(index + 2),
            _ => {}
        }
        index += 1;
    }
    None
}

fn is_escape_intermediate_byte(byte: u8) -> bool {
    (0x20..=0x2f).contains(&byte)
}

fn is_escape_final_byte(byte: u8) -> bool {
    (0x30..=0x7e).contains(&byte)
}

/// UTF-8 length of the character starting with `lead`.
fn char_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn find_escape_sequence_end_index(bytes: &[u8], start: usize) -> Option<usize> {
    let mut cursor = start;
    while cursor < bytes.len() && is_escape_intermediate_byte(bytes[cursor]) {
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }
    if is_escape_final_byte(bytes[cursor]) {
        Some(cursor + 1)
    } else {
        // ESC plus the character after it (whole, never half of a UTF-8 sequence).
        Some(start + char_len(bytes[start]))
    }
}

/// `sanitizeTerminalHistoryChunk(pendingControlSequence, data)`.
pub fn sanitize_terminal_history_chunk(pending_control_sequence: &str, data: &str) -> SanitizedChunk {
    let input = format!("{pending_control_sequence}{data}");
    let bytes = input.as_bytes();
    let mut visible = String::with_capacity(input.len());
    let mut index = 0;
    // Plain text is copied in runs, not character by character.
    let mut run_start = 0;

    macro_rules! pending_from {
        ($at:expr) => {{
            visible.push_str(&input[run_start..$at]);
            return SanitizedChunk {
                visible_text: visible,
                pending_control_sequence: input[$at..].to_owned(),
            };
        }};
    }

    while index < bytes.len() {
        let byte = bytes[index];
        if byte == ESC {
            let Some(&next) = bytes.get(index + 1) else {
                pending_from!(index);
            };
            visible.push_str(&input[run_start..index]);
            match next {
                b'[' => {
                    let mut cursor = index + 2;
                    while cursor < bytes.len() && !is_csi_final_byte(bytes[cursor]) {
                        cursor += 1;
                    }
                    if cursor >= bytes.len() {
                        run_start = index;
                        pending_from!(index);
                    }
                    let body = &input[index + 2..cursor];
                    if !should_strip_csi_sequence(body, bytes[cursor]) {
                        visible.push_str(&input[index..=cursor]);
                    }
                    index = cursor + 1;
                }
                b']' | b'P' | b'^' | b'_' => {
                    let Some(terminator) = find_string_terminator_index(bytes, index + 2) else {
                        run_start = index;
                        pending_from!(index);
                    };
                    let content = strip_string_terminator(&input[index + 2..terminator]);
                    let strip = (next == b']' && should_strip_osc_sequence(content)) || (next == b'P' && should_strip_dcs_sequence(content));
                    if !strip {
                        visible.push_str(&input[index..terminator]);
                    }
                    index = terminator;
                }
                _ => {
                    let Some(end) = find_escape_sequence_end_index(bytes, index + 1) else {
                        run_start = index;
                        pending_from!(index);
                    };
                    visible.push_str(&input[index..end]);
                    index = end;
                }
            }
            run_start = index;
            continue;
        }

        if let Some(c1) = c1_at(bytes, index) {
            match c1 {
                0x9b => {
                    visible.push_str(&input[run_start..index]);
                    let mut cursor = index + 2;
                    while cursor < bytes.len() && !is_csi_final_byte(bytes[cursor]) {
                        cursor += 1;
                    }
                    if cursor >= bytes.len() {
                        run_start = index;
                        pending_from!(index);
                    }
                    let body = &input[index + 2..cursor];
                    if !should_strip_csi_sequence(body, bytes[cursor]) {
                        visible.push_str(&input[index..=cursor]);
                    }
                    index = cursor + 1;
                    run_start = index;
                    continue;
                }
                0x9d | 0x90 | 0x9e | 0x9f => {
                    visible.push_str(&input[run_start..index]);
                    let Some(terminator) = find_string_terminator_index(bytes, index + 2) else {
                        run_start = index;
                        pending_from!(index);
                    };
                    let content = strip_string_terminator(&input[index + 2..terminator]);
                    let strip = (c1 == 0x9d && should_strip_osc_sequence(content)) || (c1 == 0x90 && should_strip_dcs_sequence(content));
                    if !strip {
                        visible.push_str(&input[index..terminator]);
                    }
                    index = terminator;
                    run_start = index;
                    continue;
                }
                _ => {}
            }
        }

        index += 1;
    }

    visible.push_str(&input[run_start..]);
    SanitizedChunk {
        visible_text: visible,
        pending_control_sequence: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> String {
        let mut pending = String::new();
        let mut out = String::new();
        for chunk in chunks {
            let result = sanitize_terminal_history_chunk(&pending, chunk);
            out.push_str(&result.visible_text);
            pending = result.pending_control_sequence;
        }
        out
    }

    // The cases of Manager.test.ts.
    #[test]
    fn strips_replay_unsafe_query_and_reply_sequences() {
        assert_eq!(
            run(&[
                "prompt ",
                "\u{1b}[32mok\u{1b}[0m ",
                "\u{1b}]11;rgb:ffff/ffff/ffff\u{7}",
                "\u{1b}[1;1R",
                "done\n"
            ]),
            "prompt \u{1b}[32mok\u{1b}[0m done\n"
        );
    }

    #[test]
    fn strips_csi_and_dcs_traffic_while_preserving_setters() {
        assert_eq!(
            run(&[
                "prompt ",
                "\u{1b}[?2026$p\u{1b}[?2026;2$y\u{1b}[>q\u{1b}[?u\u{1b}[?31u",
                "\u{1b}P$q m\u{1b}\\\u{1b}P1$r0m\u{1b}\\",
                "\u{1b}P+q544e\u{1b}\\\u{1b}P1+r544e=1b\u{1b}\\",
                "\u{90}$q m\u{9c}\u{90}1$r0m\u{9c}",
                "\u{90}+q544e\u{9c}\u{90}1+r544e=1b\u{9c}",
                "\u{1b}[!p\u{1b}[\"p\u{1b}[4 q\u{1b}[u",
                "done\n",
            ]),
            "prompt \u{1b}[!p\u{1b}[\"p\u{1b}[4 q\u{1b}[udone\n"
        );
    }

    #[test]
    fn handles_sequences_split_across_chunks() {
        assert_eq!(
            run(&[
                "before ",
                "\u{1b}[?2026$",
                "pafter ",
                "\u{1b}P$q ",
                "m\u{1b}",
                "\\after ",
                "\u{9b}?3",
                "1uafter ",
                "\u{90}+q544e",
                "\u{9c}after\n",
            ]),
            "before after after after after\n"
        );
        assert_eq!(
            run(&[
                "before clear\n",
                "\u{1b}[H\u{1b}[2J",
                "prompt ",
                "\u{1b}]11;",
                "rgb:ffff/ffff/ffff\u{7}\u{1b}[1;1",
                "R\u{1b}[36mdone\u{1b}[0m\n",
            ]),
            "before clear\n\u{1b}[H\u{1b}[2Jprompt \u{1b}[36mdone\u{1b}[0m\n"
        );
    }

    #[test]
    fn keeps_escape_sequences_with_intermediates_whole() {
        assert_eq!(run(&["before ", "\u{1b}(B", "after\n"]), "before \u{1b}(Bafter\n");
        assert_eq!(run(&["before ", "\u{1b}(", "Bafter\n"]), "before \u{1b}(Bafter\n");
        assert_eq!(run(&["\u{1b}é!"]), "\u{1b}é!");
    }
}
