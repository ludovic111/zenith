//! `JSON.parse` for usage records, keeping only the fields a reader consumes.
//!
//! The TS readers `JSON.parse` small lines and stream lines above 8 MiB through a projecting
//! reader that keeps the selected paths (`usageTranscriptReader.ts` `USAGE_FIELDS`). This is
//! both at once: a strict JSON reader over bytes that validates the whole document like
//! `JSON.parse` (trailing junk, truncation, control characters in strings all reject it) but
//! only materializes the selected paths; everything else is skipped without allocating, and
//! skipping is iterative, so deeply nested tool output never exhausts the stack.
//!
//! What it keeps follows `JSON.parse` semantics where serde_json would differ:
//! - a repeated key keeps its first position and its last value (`{"a":1,"a":2}` is `{a: 2}`);
//! - numbers out of the double range become `±Infinity` (`1e400`), not an error;
//! - object keys iterate like a JS object: array-index keys first, ascending, then insertion
//!   order ([`Obj::entries`]);
//! - invalid UTF-8 inside strings reads as U+FFFD (the bytes were decoded with
//!   `Buffer.toString("utf8")` first), and a lone surrogate escape too.

use std::borrow::Cow;

use indexmap::IndexMap;

/// A parsed JSON value, JS-shaped (numbers are doubles).
#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Obj),
}

/// A JSON object: insertion order, repeated keys replaced in place.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Obj(pub IndexMap<String, J>);

impl Obj {
    pub fn get(&self, key: &str) -> Option<&J> {
        self.0.get(key)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    /// `Object.entries` order: canonical array-index keys ascending, then the rest in
    /// insertion order.
    pub fn entries(&self) -> Vec<(&str, &J)> {
        let mut indexed: Vec<(u32, &str, &J)> = Vec::new();
        let mut named: Vec<(&str, &J)> = Vec::new();
        for (key, value) in &self.0 {
            match array_index(key) {
                Some(index) => indexed.push((index, key, value)),
                None => named.push((key, value)),
            }
        }
        if indexed.is_empty() {
            return named;
        }
        indexed.sort_by_key(|(index, _, _)| *index);
        indexed.into_iter().map(|(_, key, value)| (key, value)).chain(named).collect()
    }
}

/// A canonical array index (`"0"`, `"12"`, not `"012"`), below 2^32 - 1.
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || (key.len() > 1 && key.starts_with('0')) || !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u64>().ok().filter(|value| *value < u64::from(u32::MAX)).map(|value| value as u32)
}

impl J {
    /// `value[key]` (undefined for non-objects).
    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(object) => object.get(key),
            _ => None,
        }
    }

    /// `typeof value === "object" && value !== null` (arrays included).
    pub fn is_object_like(&self) -> bool {
        matches!(self, J::Obj(_) | J::Arr(_))
    }

    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            J::Obj(object) => Some(object),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[J]> {
        match self {
            J::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            J::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    /// `typeof value === "number"`.
    pub fn as_num(&self) -> Option<f64> {
        match self {
            J::Num(value) => Some(*value),
            _ => None,
        }
    }

    /// `typeof value === "number" && Number.isFinite(value)`.
    pub fn as_finite(&self) -> Option<f64> {
        self.as_num().filter(|value| value.is_finite())
    }
}

/// `value?.[key]` through an optional value.
pub fn get<'a>(value: Option<&'a J>, key: &str) -> Option<&'a J> {
    value.and_then(|value| value.get(key))
}

/// Which paths of a document to keep.
#[derive(Debug)]
pub enum Sel {
    /// Keep this value whole.
    All,
    /// Keep only these object fields (a non-object value is kept whole).
    Fields(&'static [(&'static str, Sel)]),
}

const MAX_KEPT_DEPTH: usize = 2048;

/// Parses a whole document, keeping the selected paths. `None` where `JSON.parse` throws.
pub fn parse_selected(bytes: &[u8], selection: &Sel) -> Option<J> {
    let mut parser = Parser { b: bytes, i: 0 };
    parser.ws();
    let value = parser.value(selection, 0).ok()?;
    parser.ws();
    (parser.i == bytes.len()).then_some(value)
}

/// `JSON.parse(text)`.
pub fn parse(bytes: &[u8]) -> Option<J> {
    parse_selected(bytes, &Sel::All)
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

type R<T> = Result<T, ()>;

impl<'a> Parser<'a> {
    #[inline]
    fn ws(&mut self) {
        while let Some(&byte) = self.b.get(self.i) {
            if matches!(byte, b' ' | b'\t' | b'\n' | b'\r') {
                self.i += 1;
            } else {
                break;
            }
        }
    }

    #[inline]
    fn peek(&self) -> R<u8> {
        self.b.get(self.i).copied().ok_or(())
    }

    #[inline]
    fn expect(&mut self, byte: u8) -> R<()> {
        if self.peek()? == byte {
            self.i += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn value(&mut self, selection: &Sel, depth: usize) -> R<J> {
        if depth > MAX_KEPT_DEPTH {
            return Err(());
        }
        match self.peek()? {
            b'{' => self.object(selection, depth),
            b'[' => self.array(depth),
            b'"' => {
                self.i += 1;
                Ok(J::Str(self.string()?))
            }
            b't' => self.literal(b"true").map(|()| J::Bool(true)),
            b'f' => self.literal(b"false").map(|()| J::Bool(false)),
            b'n' => self.literal(b"null").map(|()| J::Null),
            _ => {
                let start = self.i;
                self.number()?;
                // The grammar was checked: ASCII digits, sign, dot and exponent only.
                let text = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| ())?;
                text.parse::<f64>().map(J::Num).map_err(|_| ())
            }
        }
    }

    fn object(&mut self, selection: &Sel, depth: usize) -> R<J> {
        self.i += 1;
        let mut object = IndexMap::new();
        self.ws();
        if self.peek()? == b'}' {
            self.i += 1;
            return Ok(J::Obj(Obj(object)));
        }
        loop {
            self.ws();
            self.expect(b'"')?;
            let key = self.key()?;
            self.ws();
            self.expect(b':')?;
            self.ws();
            let child = match selection {
                Sel::All => Some(&Sel::All),
                Sel::Fields(fields) => fields.iter().find(|(name, _)| name.as_bytes() == key.as_ref()).map(|(_, child)| child),
            };
            match child {
                Some(child) => {
                    let value = self.value(child, depth + 1)?;
                    let key = match key {
                        Cow::Borrowed(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                        Cow::Owned(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    };
                    object.insert(key, value);
                }
                None => self.skip_value()?,
            }
            self.ws();
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Ok(J::Obj(Obj(object)));
                }
                _ => return Err(()),
            }
        }
    }

    fn array(&mut self, depth: usize) -> R<J> {
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.peek()? == b']' {
            self.i += 1;
            return Ok(J::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value(&Sel::All, depth + 1)?);
            self.ws();
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Ok(J::Arr(items));
                }
                _ => return Err(()),
            }
        }
    }

    fn literal(&mut self, word: &[u8]) -> R<()> {
        if self.b.get(self.i..self.i + word.len()) == Some(word) {
            self.i += word.len();
            Ok(())
        } else {
            Err(())
        }
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`
    fn number(&mut self) -> R<()> {
        if self.peek()? == b'-' {
            self.i += 1;
        }
        match self.peek()? {
            b'0' => self.i += 1,
            b'1'..=b'9' => self.digits(),
            _ => return Err(()),
        }
        if self.b.get(self.i) == Some(&b'.') {
            self.i += 1;
            if !matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                return Err(());
            }
            self.digits();
        }
        if matches!(self.b.get(self.i), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.b.get(self.i), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if !matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
                return Err(());
            }
            self.digits();
        }
        Ok(())
    }

    #[inline]
    fn digits(&mut self) {
        while matches!(self.b.get(self.i), Some(b'0'..=b'9')) {
            self.i += 1;
        }
    }

    /// A key after its opening quote: borrowed when it has no escapes.
    fn key(&mut self) -> R<Cow<'a, [u8]>> {
        let start = self.i;
        loop {
            let byte = self.peek()?;
            match byte {
                b'"' => {
                    let key = &self.b[start..self.i];
                    self.i += 1;
                    return Ok(Cow::Borrowed(key));
                }
                b'\\' => {
                    self.i = start;
                    return self.string_bytes().map(Cow::Owned);
                }
                0..=0x1f => return Err(()),
                _ => self.i += 1,
            }
        }
    }

    /// A string after its opening quote.
    fn string(&mut self) -> R<String> {
        let bytes = self.string_bytes()?;
        Ok(match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        })
    }

    fn string_bytes(&mut self) -> R<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let run_start = self.i;
            while let Some(&byte) = self.b.get(self.i) {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.i += 1;
            }
            out.extend_from_slice(&self.b[run_start..self.i]);
            match self.peek()? {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    let escape = self.peek()?;
                    self.i += 1;
                    match escape {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let unit = self.hex4()?;
                            let ch = if (0xd800..0xdc00).contains(&unit) {
                                // A high surrogate pairs with an immediately following low one.
                                if self.b.get(self.i..self.i + 2) == Some(b"\\u") {
                                    let save = self.i;
                                    self.i += 2;
                                    let low = self.hex4()?;
                                    if (0xdc00..0xe000).contains(&low) {
                                        char::from_u32(0x10000 + ((unit - 0xd800) << 10) + (low - 0xdc00)).unwrap_or('\u{fffd}')
                                    } else {
                                        self.i = save;
                                        '\u{fffd}'
                                    }
                                } else {
                                    '\u{fffd}'
                                }
                            } else {
                                char::from_u32(unit).unwrap_or('\u{fffd}')
                            };
                            let mut buffer = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buffer).as_bytes());
                        }
                        _ => return Err(()),
                    }
                }
                _ => return Err(()),
            }
        }
    }

    fn hex4(&mut self) -> R<u32> {
        let digits = self.b.get(self.i..self.i + 4).ok_or(())?;
        let mut value = 0u32;
        for &digit in digits {
            value = value * 16 + (digit as char).to_digit(16).ok_or(())?;
        }
        self.i += 4;
        Ok(value)
    }

    /// Validates and skips a string after its opening quote, without allocating.
    fn skip_string(&mut self) -> R<()> {
        loop {
            let byte = *self.b.get(self.i).ok_or(())?;
            self.i += 1;
            match byte {
                b'"' => return Ok(()),
                b'\\' => {
                    let escape = self.peek()?;
                    self.i += 1;
                    match escape {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => {}
                        b'u' => {
                            self.hex4()?;
                        }
                        _ => return Err(()),
                    }
                }
                0..=0x1f => return Err(()),
                _ => {}
            }
        }
    }

    /// Validates and skips one value, iteratively (no recursion, so no depth limit).
    fn skip_value(&mut self) -> R<()> {
        let mut stack: Vec<u8> = Vec::new();
        loop {
            // A value starts here.
            self.ws();
            match self.peek()? {
                b'{' => {
                    self.i += 1;
                    self.ws();
                    if self.peek()? == b'}' {
                        self.i += 1;
                    } else {
                        stack.push(b'{');
                        self.member_key()?;
                        continue;
                    }
                }
                b'[' => {
                    self.i += 1;
                    self.ws();
                    if self.peek()? == b']' {
                        self.i += 1;
                    } else {
                        stack.push(b'[');
                        continue;
                    }
                }
                b'"' => {
                    self.i += 1;
                    self.skip_string()?;
                }
                b't' => self.literal(b"true")?,
                b'f' => self.literal(b"false")?,
                b'n' => self.literal(b"null")?,
                _ => self.number()?,
            }
            // A value ended: close containers until one continues.
            loop {
                let Some(&top) = stack.last() else {
                    return Ok(());
                };
                self.ws();
                let byte = self.peek()?;
                self.i += 1;
                match (top, byte) {
                    (b'{', b',') => {
                        self.ws();
                        self.member_key()?;
                        break;
                    }
                    (b'[', b',') => break,
                    (b'{', b'}') | (b'[', b']') => {
                        stack.pop();
                    }
                    _ => return Err(()),
                }
            }
        }
    }

    /// `"key" :` inside an object being skipped.
    fn member_key(&mut self) -> R<()> {
        self.expect(b'"')?;
        self.skip_string()?;
        self.ws();
        self.expect(b':')
    }
}

/// `JSON.stringify(value)`.
pub fn stringify(value: &J) -> String {
    let mut out = String::new();
    write_json(&mut out, value);
    out
}

fn write_json(out: &mut String, value: &J) {
    match value {
        J::Null => out.push_str("null"),
        J::Bool(true) => out.push_str("true"),
        J::Bool(false) => out.push_str("false"),
        J::Num(number) => out.push_str(&zc_providers::js_json::format_js_number(*number)),
        J::Str(text) => zc_providers::js_json::write_string(out, text),
        J::Arr(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(out, item);
            }
            out.push(']');
        }
        J::Obj(object) => {
            out.push('{');
            for (index, (key, item)) in object.entries().into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                zc_providers::js_json::write_string(out, key);
                out.push(':');
                write_json(out, item);
            }
            out.push('}');
        }
    }
}

/// `JSON.stringify(text)`.
pub fn quote(text: &str) -> String {
    let mut out = String::new();
    zc_providers::js_json::write_string(&mut out, text);
    out
}

/// `String(number)`.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        zc_providers::js_json::format_js_number(value)
    }
}

/// A JS number as a JSON value: integral values below 2^53 as integers (so they print as
/// `JSON.stringify` prints them), non-finite values as `null`.
pub fn num(value: f64) -> serde_json::Value {
    const SAFE: f64 = 9_007_199_254_740_992.0;
    if value.is_finite() && value.fract() == 0.0 && value.abs() < SAFE {
        #[allow(clippy::cast_possible_truncation)]
        let integer = value as i64;
        serde_json::Value::from(integer)
    } else {
        serde_json::Number::from_f64(value).map_or(serde_json::Value::Null, serde_json::Value::Number)
    }
}

/// A [`J`] as a `serde_json` value (non-finite numbers become `null`, as `JSON.stringify`).
pub fn to_value(value: &J) -> serde_json::Value {
    match value {
        J::Null => serde_json::Value::Null,
        J::Bool(flag) => serde_json::Value::Bool(*flag),
        J::Num(number) => num(*number),
        J::Str(text) => serde_json::Value::String(text.clone()),
        J::Arr(items) => serde_json::Value::Array(items.iter().map(to_value).collect()),
        J::Obj(object) => serde_json::Value::Object(object.entries().into_iter().map(|(key, item)| (key.to_owned(), to_value(item))).collect()),
    }
}

/// JS `StringToNumber` (`Number(text)`): trimmed decimal, `Infinity`, or `0x`/`0o`/`0b`
/// integers; `NaN` for anything else.
pub fn string_to_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(is_js_whitespace);
    if trimmed.is_empty() {
        return 0.0;
    }
    let radix = match trimmed.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &trimmed[2..];
        if digits.is_empty() {
            return f64::NAN;
        }
        let mut value = 0f64;
        for ch in digits.chars() {
            let Some(digit) = ch.to_digit(radix) else {
                return f64::NAN;
            };
            value = value * f64::from(radix) + f64::from(digit);
        }
        return value;
    }
    let (sign, unsigned) = match trimmed.as_bytes()[0] {
        b'+' => (1.0, &trimmed[1..]),
        b'-' => (-1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    if unsigned == "Infinity" {
        return sign * f64::INFINITY;
    }
    // StrUnsignedDecimalLiteral: digits [. digits] | . digits, then an optional exponent.
    let bytes = unsigned.as_bytes();
    let mut index = 0;
    let int_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let int_digits = index - int_start;
    let mut frac_digits = 0;
    if index < bytes.len() && bytes[index] == b'.' {
        index += 1;
        let frac_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        frac_digits = index - frac_start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return f64::NAN;
    }
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        index += 1;
        if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
            index += 1;
        }
        let exp_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == exp_start {
            return f64::NAN;
        }
    }
    if index != bytes.len() {
        return f64::NAN;
    }
    // Rust's parser accepts "5." and ".5" like JS once the grammar is checked.
    unsigned.parse::<f64>().map_or(f64::NAN, |value| sign * value)
}

/// JS `WhiteSpace` and `LineTerminator` (what `String.prototype.trim` removes).
pub fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `text.trim()`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: Sel = Sel::Fields(&[("type", Sel::All), ("message", Sel::Fields(&[("usage", Sel::All)]))]);

    #[test]
    fn keeps_selected_fields_and_validates_the_rest() {
        let line = "{\"padding\":[1,{\"a\":\"\\u00e9\\\\\"}],\"type\":\"assistant\",\"message\":{\"content\":\"x\",\"usage\":{\"input_tokens\":3}}}".as_bytes();
        let value = parse_selected(line, &CLAUDE).unwrap();
        assert_eq!(value.get("type").and_then(J::as_str), Some("assistant"));
        assert!(value.get("padding").is_none());
        assert!(get(value.get("message"), "content").is_none());
        assert_eq!(get(get(value.get("message"), "usage"), "input_tokens").and_then(J::as_num), Some(3.0));
    }

    #[test]
    fn rejects_what_json_parse_rejects() {
        for bad in [
            &b"{\"a\":1} junk"[..],
            b"{\"a\":1",
            b"{\"a\":1}{\"a\":1}",
            b"{\"a\":01}",
            b"{\"a\":1.}",
            b"{\"a\":\"\x01\"}",
            b"{\"a\":[1,]}",
            b"{,}",
            b"\xef\xbb\xbf{}",
            b"",
        ] {
            assert!(parse_selected(bad, &CLAUDE).is_none(), "{}", String::from_utf8_lossy(bad));
            assert!(parse(bad).is_none());
        }
    }

    #[test]
    fn json_parse_semantics() {
        let value = parse("{\"b\":1,\"2\":2,\"a\":3,\"b\":4,\"1\":5,\"x\":1e400,\"s\":\"\\ud83d\\ude00\\ud800\"}".as_bytes()).unwrap();
        let object = value.as_obj().unwrap();
        let keys: Vec<&str> = object.entries().into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["1", "2", "b", "a", "x", "s"]);
        assert_eq!(object.get("b"), Some(&J::Num(4.0)));
        assert_eq!(object.get("x").and_then(J::as_num), Some(f64::INFINITY));
        assert_eq!(object.get("s").and_then(J::as_str), Some("😀\u{fffd}"));
        assert_eq!(stringify(&value), r#"{"1":5,"2":2,"b":4,"a":3,"x":null,"s":"😀�"}"#);
    }

    #[test]
    fn deep_unselected_nesting_is_skipped() {
        let mut line = String::from("{\"deep\":");
        line.push_str(&"[".repeat(100_000));
        line.push('0');
        line.push_str(&"]".repeat(100_000));
        line.push_str(",\"type\":\"assistant\"}");
        assert_eq!(
            parse_selected(line.as_bytes(), &CLAUDE).unwrap().get("type").and_then(J::as_str),
            Some("assistant")
        );
    }

    #[test]
    fn string_to_number_matches_js() {
        assert_eq!(string_to_number(" 12 "), 12.0);
        assert_eq!(string_to_number("0x10"), 16.0);
        assert_eq!(string_to_number("1e3"), 1000.0);
        assert_eq!(string_to_number(".5"), 0.5);
        assert_eq!(string_to_number("-Infinity"), f64::NEG_INFINITY);
        assert!(string_to_number("12abc").is_nan());
        assert!(string_to_number("inf").is_nan());
        assert!(string_to_number("-0x10").is_nan());
    }
}
