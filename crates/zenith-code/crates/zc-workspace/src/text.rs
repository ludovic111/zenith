//! JS string semantics the content search depends on: UTF-16 lengths and indices, the
//! byte-offset → string-index conversion of `mapContentMatchRanges`, and the whole-word rule of
//! `isWholeWordRange` (`/[\p{Letter}\p{Mark}\p{Number}_]/u` around the match edges).

use std::sync::OnceLock;

use regex::Regex;

pub use zc_core::defect::js_length;

/// `Buffer.from(line).subarray(0, byteOffset).toString().length`: the UTF-16 length of the
/// line's first `byte_offset` bytes, decoded leniently (a cut code point counts as one U+FFFD).
pub fn byte_offset_to_utf16_index(line: &str, byte_offset: usize) -> usize {
    let bytes = line.as_bytes();
    let end = byte_offset.min(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).encode_utf16().count()
}

/// A line as UTF-16 code units, for code-point lookups at JS string indices.
pub struct Utf16Line {
    units: Vec<u16>,
}

impl Utf16Line {
    pub fn new(line: &str) -> Self {
        Self {
            units: line.encode_utf16().collect(),
        }
    }

    /// `line.length`.
    pub fn len(&self) -> usize {
        self.units.len()
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// `line.codePointAt(index)` as a char; `None` past the end or on a lone surrogate.
    pub fn code_point_at(&self, index: usize) -> Option<char> {
        let first = *self.units.get(index)?;
        if (0xD800..0xDC00).contains(&first) {
            if let Some(&second) = self.units.get(index + 1) {
                if (0xDC00..0xE000).contains(&second) {
                    let code = 0x10000 + (((first as u32) - 0xD800) << 10) + ((second as u32) - 0xDC00);
                    return char::from_u32(code);
                }
            }
            return None;
        }
        char::from_u32(first as u32)
    }

    /// `codePointBefore(line, index)`: the code point ending just before `index`.
    pub fn code_point_before(&self, index: usize) -> Option<char> {
        if index == 0 {
            return None;
        }
        let previous = *self.units.get(index - 1)?;
        let previous_index = if (0xDC00..0xE000).contains(&previous) && index >= 2 {
            index - 2
        } else {
            index - 1
        };
        self.code_point_at(previous_index)
    }
}

fn word_character() -> &'static Regex {
    static WORD: OnceLock<Regex> = OnceLock::new();
    WORD.get_or_init(|| Regex::new(r"^[\p{Letter}\p{Mark}\p{Number}_]$").expect("valid word regex"))
}

/// `WORD_CHARACTER.test(character)`.
pub fn is_word_character(character: Option<char>) -> bool {
    character.is_some_and(|character| {
        let mut buffer = [0u8; 4];
        word_character().is_match(character.encode_utf8(&mut buffer))
    })
}

/// `isWholeWordRange(line, {start, end})`, on UTF-16 indices. Matching VS Code, an edge is a
/// boundary when it touches the line edge, when the neighbouring character is not a word
/// character, or when the match's own edge character is not a word character.
pub fn is_whole_word_range(line: &Utf16Line, start: usize, end: usize) -> bool {
    if end <= start {
        return false;
    }
    let left = start == 0 || !is_word_character(line.code_point_before(start)) || !is_word_character(line.code_point_at(start));
    let right = end >= line.len() || !is_word_character(line.code_point_at(end)) || !is_word_character(line.code_point_before(end));
    left && right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offsets_become_string_indices() {
        let line = "const label = \"héllo wörld\";";
        let start = line.find("wörld").unwrap();
        let end = start + "wörld".len();
        let (start16, end16) = (byte_offset_to_utf16_index(line, start), byte_offset_to_utf16_index(line, end));
        let units: Vec<u16> = line.encode_utf16().collect();
        assert_eq!(String::from_utf16(&units[start16..end16]).unwrap(), "wörld");
        // Astral characters count two code units.
        assert_eq!(byte_offset_to_utf16_index("𐐀foo", 4), 2);
        // Offsets past the end clamp, a cut code point counts once.
        assert_eq!(byte_offset_to_utf16_index("é", 1), 1);
        assert_eq!(byte_offset_to_utf16_index("ab", 10), 2);
    }

    #[test]
    fn whole_word_rules() {
        let line = Utf16Line::new("note notes denote");
        assert!(is_whole_word_range(&line, 0, 4));
        assert!(!is_whole_word_range(&line, 5, 9));
        assert!(!is_whole_word_range(&line, 13, 17));
        let astral = Utf16Line::new("𐐀foo foo foo𐐀");
        assert!(!is_whole_word_range(&astral, 2, 5));
        assert!(is_whole_word_range(&astral, 6, 9));
        assert!(!is_whole_word_range(&astral, 10, 13));
        let punctuation = Utf16Line::new("-foo- -foo- -foo-");
        assert!(is_whole_word_range(&punctuation, 0, 5));
        assert!(is_whole_word_range(&punctuation, 6, 11));
        let regex = Utf16Line::new("afoo-b");
        assert!(!is_whole_word_range(&regex, 1, 5));
        assert!(!is_whole_word_range(&regex, 3, 3));
        assert!(is_word_character(Some('_')));
        assert!(is_word_character(Some('\u{0301}')), "combining marks are word characters");
        assert!(!is_word_character(Some('-')));
    }
}
