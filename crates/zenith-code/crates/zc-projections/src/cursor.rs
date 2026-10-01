//! `orchestration/threadDetailCursor.ts`: the opaque, exclusive cursor of windowed thread
//! detail reads. It names the thread and the keyset boundary `(anchor, turn id)` of a page
//! already delivered, so it survives row-id rewrites and projection rebuilds.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadDetailPageCursor {
    pub thread_id: String,
    pub before_anchor_at: String,
    /// `""` for the rare turn row with a null turn id.
    pub before_turn_id: String,
}

/// `encodeThreadDetailPageCursor`: base64url (no padding) of `{"t","a","i"}`.
pub fn encode_thread_detail_page_cursor(cursor: &ThreadDetailPageCursor) -> String {
    let text = json!({
        "t": cursor.thread_id,
        "a": cursor.before_anchor_at,
        "i": cursor.before_turn_id,
    })
    .to_string();
    URL_SAFE_NO_PAD.encode(text)
}

/// Node's lenient `Buffer.from(text, "base64url")`: both alphabets, padding and anything else
/// outside the alphabet ignored, a dangling sixth bit group dropped.
fn decode_base64url_lenient(encoded: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in encoded.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '-' | '+' => 62,
            '_' | '/' => 63,
            '=' => break,
            _ => continue,
        };
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    out
}

/// `decodeThreadDetailPageCursor`: `None` for anything that is not a well-formed cursor.
pub fn decode_thread_detail_page_cursor(encoded: &str) -> Option<ThreadDetailPageCursor> {
    let bytes = decode_base64url_lenient(encoded);
    // `toString("utf8")` replaces invalid sequences instead of failing.
    let text = String::from_utf8_lossy(&bytes);
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let record = parsed.as_object()?;
    let thread_id = record.get("t")?.as_str().filter(|t| !t.is_empty())?;
    // Empty strings are valid boundary values, not malformed input.
    let anchor = record.get("a")?.as_str()?;
    let turn_id = record.get("i")?.as_str()?;
    Some(ThreadDetailPageCursor {
        thread_id: thread_id.to_string(),
        before_anchor_at: anchor.to_string(),
        before_turn_id: turn_id.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor() -> ThreadDetailPageCursor {
        ThreadDetailPageCursor {
            thread_id: "thread-1".into(),
            before_anchor_at: "2026-02-01T00:00:00.000Z".into(),
            before_turn_id: "turn-1".into(),
        }
    }

    // threadDetailCursor.test.ts
    #[test]
    fn round_trips() {
        let encoded = encode_thread_detail_page_cursor(&cursor());
        assert!(!encoded.contains('='));
        assert_eq!(decode_thread_detail_page_cursor(&encoded), Some(cursor()));
    }

    #[test]
    fn matches_node_encoding() {
        // Buffer.from('{"t":"thread-1","a":"2026-02-01T00:00:00.000Z","i":"turn-1"}').toString("base64url")
        assert_eq!(
            encode_thread_detail_page_cursor(&cursor()),
            "eyJ0IjoidGhyZWFkLTEiLCJhIjoiMjAyNi0wMi0wMVQwMDowMDowMC4wMDBaIiwiaSI6InR1cm4tMSJ9"
        );
    }

    #[test]
    fn accepts_empty_boundaries() {
        let empty = ThreadDetailPageCursor {
            thread_id: "thread-1".into(),
            before_anchor_at: String::new(),
            before_turn_id: String::new(),
        };
        let encoded = encode_thread_detail_page_cursor(&empty);
        assert_eq!(decode_thread_detail_page_cursor(&encoded), Some(empty));
    }

    #[test]
    fn rejects_malformed_cursors() {
        assert_eq!(decode_thread_detail_page_cursor("not-a-cursor"), None);
        assert_eq!(decode_thread_detail_page_cursor(""), None);
        let encode = |text: &str| URL_SAFE_NO_PAD.encode(text);
        assert_eq!(decode_thread_detail_page_cursor(&encode("[]")), None);
        assert_eq!(decode_thread_detail_page_cursor(&encode("null")), None);
        assert_eq!(decode_thread_detail_page_cursor(&encode(r#"{"t":"","a":"x","i":"y"}"#)), None);
        assert_eq!(decode_thread_detail_page_cursor(&encode(r#"{"t":"t","a":1,"i":"y"}"#)), None);
        assert_eq!(decode_thread_detail_page_cursor(&encode(r#"{"t":"t","a":"x"}"#)), None);
    }
}
