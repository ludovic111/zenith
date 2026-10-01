//! Helpers of `@t3tools/shared` and of the JavaScript runtime that the Azure DevOps modules lean
//! on and that nothing else in the workspace ports yet: `gitPatchPath.ts` and
//! `String.prototype.localeCompare` for the timestamps the conversation is sorted by.

use std::cmp::Ordering;

/// What git escapes by name (`ESCAPE_BY_CHARACTER`). Not every byte git would escape:
/// `core.quotePath` also escapes anything outside ASCII, which is a terminal setting rather than
/// anything the format needs.
fn named_escape(character: char) -> Option<&'static str> {
    Some(match character {
        '"' => "\\\"",
        '\\' => "\\\\",
        '\u{7}' => "\\a",
        '\u{8}' => "\\b",
        '\t' => "\\t",
        '\n' => "\\n",
        '\u{b}' => "\\v",
        '\u{c}' => "\\f",
        '\r' => "\\r",
        _ => return None,
    })
}

/// `quoteGitPatchPath`: a name as a patch header can carry it, itself where that is unambiguous
/// and git's C-style quoted form where it is not (a quote, a backslash or a control character).
/// A header side's `a/` or `b/` belongs inside the quoting, so pass it in with the name.
pub fn quote_git_patch_path(path: &str) -> String {
    let mut body = String::with_capacity(path.len());
    let mut quoting = false;
    for character in path.chars() {
        if let Some(escape) = named_escape(character) {
            body.push_str(escape);
            quoting = true;
            continue;
        }
        let code = character as u32;
        // A control character is one byte in UTF-8, so its code point is the byte git writes.
        if code < 0x20 || code == 0x7f {
            body.push_str(&format!("\\{code:03o}"));
            quoting = true;
            continue;
        }
        body.push(character);
    }
    if quoting {
        format!("\"{body}\"")
    } else {
        path.to_owned()
    }
}

/// `unescapeBody`: the escapes of a quoted form undone, per byte, then decoded as UTF-8.
fn unescape_body(body: &str) -> String {
    if !body.contains('\\') {
        return body.to_owned();
    }
    let chars: Vec<char> = body.chars().collect();
    let mut bytes: Vec<u8> = Vec::with_capacity(body.len());
    let mut at = 0;
    while at < chars.len() {
        let character = chars[at];
        if character != '\\' {
            let mut buffer = [0u8; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            at += 1;
            continue;
        }
        let Some(&escaped) = chars.get(at + 1) else {
            bytes.push(b'\\');
            break;
        };
        let named = match escaped {
            '"' => Some(0x22),
            '\\' => Some(0x5c),
            'a' => Some(0x07),
            'b' => Some(0x08),
            'f' => Some(0x0c),
            'n' => Some(0x0a),
            'r' => Some(0x0d),
            't' => Some(0x09),
            'v' => Some(0x0b),
            _ => None,
        };
        if let Some(byte) = named {
            bytes.push(byte);
            at += 2;
            continue;
        }
        let octal: String = chars.iter().skip(at + 1).take(3).collect();
        if octal.len() == 3 && octal.chars().all(|c| ('0'..='7').contains(&c)) {
            bytes.push(u32::from_str_radix(&octal, 8).unwrap_or(0) as u8);
            at += 4;
            continue;
        }
        let mut buffer = [0u8; 4];
        bytes.extend_from_slice(escaped.encode_utf8(&mut buffer).as_bytes());
        at += 2;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `unquoteGitPatchPath`: one header's name token as the name it stands for. The escapes are
/// undone whether the quotes are still there or not.
pub fn unquote_git_patch_path(token: &str) -> String {
    if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') {
        return unescape_body(&token[1..token.len() - 1]);
    }
    unescape_body(token)
}

/// The primary weight of one character under the CLDR root collation that `localeCompare` uses
/// (non-ignorable punctuation): white space, then punctuation and symbols in CLDR's order, then
/// digits, then letters case-insensitively. Anything else sorts after, by code point.
fn primary_weight(character: char) -> (u32, u32) {
    const PUNCTUATION: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    if character.is_whitespace() {
        return (0, character as u32);
    }
    if let Some(index) = PUNCTUATION.find(character) {
        return (1, index as u32);
    }
    if character.is_ascii_digit() {
        return (2, character as u32);
    }
    if character.is_ascii_alphabetic() {
        return (3, character.to_ascii_lowercase() as u32);
    }
    (4, character as u32)
}

/// `left.localeCompare(right)`, close enough for the host timestamps it orders: a primary
/// comparison by [`primary_weight`], then lowercase before uppercase.
pub fn locale_compare(left: &str, right: &str) -> Ordering {
    let primary = left.chars().map(primary_weight).cmp(right.chars().map(primary_weight));
    if primary != Ordering::Equal {
        return primary;
    }
    let tertiary = |c: char| u8::from(c.is_uppercase());
    left.chars().map(tertiary).cmp(right.chars().map(tertiary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_git() {
        assert_eq!(quote_git_patch_path("a/plain name.md"), "a/plain name.md");
        assert_eq!(quote_git_patch_path("a/notes\treadme.md"), "\"a/notes\\treadme.md\"");
        assert_eq!(quote_git_patch_path("a/\u{1}x\u{7f}"), "\"a/\\001x\\177\"");
        assert_eq!(quote_git_patch_path("a/é"), "a/é");
    }

    #[test]
    fn unquotes_what_it_quoted() {
        for path in ["every\t\n\"kind\"\\of.md", "plain", "\u{1}ctl", "é\tà"] {
            assert_eq!(unquote_git_patch_path(&quote_git_patch_path(path)), path);
        }
        assert_eq!(unquote_git_patch_path("\"\\303\\251\""), "é");
    }

    #[test]
    fn orders_timestamps_like_locale_compare() {
        assert_eq!(locale_compare("2026-07-02T00:00:00Z", "2026-07-03T00:00:00Z"), Ordering::Less);
        assert_eq!(locale_compare("2026-07-02T00:00:00.5Z", "2026-07-02T00:00:00Z"), Ordering::Less);
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("x", "x"), Ordering::Equal);
    }
}
