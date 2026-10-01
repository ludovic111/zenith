//! `unquoteGitPatchPath` of `packages/shared/src/gitPatchPath.ts`: how a file's name travels in a
//! unified patch. Git writes a name holding a quote, a backslash or a control character quoted,
//! with C-style escapes (and, with `core.quotePath`, every byte outside ASCII as octal).

/// The byte an escape stands for, for the escapes git writes by name.
fn named_escape(escaped: char) -> Option<u8> {
    Some(match escaped {
        '"' => 0x22,
        '\\' => 0x5c,
        'a' => 0x07,
        'b' => 0x08,
        'f' => 0x0c,
        'n' => 0x0a,
        'r' => 0x0d,
        't' => 0x09,
        'v' => 0x0b,
        _ => return None,
    })
}

/// `new TextDecoder().decode(bytes)`: UTF-8 with replacement characters, and a leading byte
/// order mark consumed.
fn decode_utf8(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// `unescapeBody`: the escapes inside a quoted form undone, whether or not the quotes are still
/// around them. The escapes are per byte, so a name in another alphabet arrives as a run of octal
/// and only reads back as itself once those bytes are rejoined and decoded together; an escape
/// git would never write reads the way C reads it.
fn unescape_body(body: &str) -> String {
    if !body.contains('\\') {
        return body.to_owned();
    }
    let chars: Vec<char> = body.chars().collect();
    let mut bytes: Vec<u8> = Vec::with_capacity(body.len());
    let mut at = 0;
    let mut buffer = [0u8; 4];
    while at < chars.len() {
        let character = chars[at];
        if character != '\\' {
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            at += 1;
            continue;
        }
        let Some(&escaped) = chars.get(at + 1) else {
            bytes.push(b'\\');
            break;
        };
        if let Some(byte) = named_escape(escaped) {
            bytes.push(byte);
            at += 2;
            continue;
        }
        let octal = chars.get(at + 1..at + 4);
        if let Some(digits) = octal.filter(|digits| digits.iter().all(|digit| ('0'..='7').contains(digit))) {
            let value = digits.iter().fold(0u32, |value, digit| value * 8 + digit.to_digit(8).unwrap_or(0));
            // A `Uint8Array` keeps the low byte of `\777`.
            bytes.push((value & 0xff) as u8);
            at += 4;
            continue;
        }
        bytes.extend_from_slice(escaped.encode_utf8(&mut buffer).as_bytes());
        at += 2;
    }
    decode_utf8(&bytes)
}

/// `unquoteGitPatchPath`: one header's name token as the name it stands for. The escapes are
/// undone whether the quotes are still there or not, because patch parsers disagree about how
/// much of the quoting they hand back.
pub fn unquote_git_patch_path(token: &str) -> String {
    if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') {
        return unescape_body(&token[1..token.len() - 1]);
    }
    unescape_body(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undoes_named_and_octal_escapes() {
        assert_eq!(unquote_git_patch_path(r#""a/we\tird.ts""#), "a/we\tird.ts");
        assert_eq!(unquote_git_patch_path(r#""caf\303\251""#), "café");
        assert_eq!(unquote_git_patch_path("plain name"), "plain name");
        assert_eq!(unquote_git_patch_path(r#""a\qb""#), "aqb");
        assert_eq!(unquote_git_patch_path(r"trailing\"), "trailing\\");
        assert_eq!(unquote_git_patch_path("\""), "\"");
    }
}
