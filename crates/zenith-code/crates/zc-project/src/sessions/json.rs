//! `project/AgentSessionJson.ts`: project one JSONL record onto the fields a schema reads,
//! while streaming, without materializing the values it does not select (a Codex record with
//! a multi-GB screenshot in a tool result costs nothing).
//!
//! The reader is a streaming JSON tokenizer (the `stream-json` 3.6 core parser with
//! `packValues: false`) feeding a value assembler only for selected paths. It matches
//! `JSON.parse` on what it keeps: the last of duplicate keys wins (at the first key's
//! position), and malformed input (a truncated record, trailing data, a bad escape, a trailing
//! comma, a root scalar) reads as `None`.
//!
//! Allocation is charged against a budget the way the TS reader charges it: every key costs
//! two bytes per UTF-16 unit; a selected token costs 64 plus two bytes per UTF-16 unit of its
//! text (a string is a start, one chunk per write it spans, and an end token). Exhausting the
//! budget or nesting deeper than 128 is a [`TranscriptJsonLimitError`], which rejects the
//! whole transcript, never a message within it.

use serde_json::{Map, Number, Value};

/// One path segment: an object key, an array index, or `null` (a value before its key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
    Null,
}

/// The selected-history budget or the depth limit ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptJsonLimitError(pub String);

impl std::fmt::Display for TranscriptJsonLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TranscriptJsonLimitError {}

/// The default nesting limit.
pub const DEFAULT_MAX_DEPTH: usize = 128;

enum Builder {
    Object(Map<String, Value>),
    Array(Vec<Value>),
}

struct Frame {
    path: Vec<PathSegment>,
    selected: bool,
    is_array: bool,
    /// The current key (objects) or index (arrays).
    key: Option<String>,
    index: usize,
    builder: Option<Builder>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expect {
    /// A value (or, right after `[`, a value or `]`).
    Value {
        or_close: bool,
    },
    /// A key (or, right after `{`, a key or `}`).
    Key {
        or_close: bool,
    },
    Colon,
    /// `,` or the closer of the current container.
    CommaOrClose,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lexeme {
    None,
    /// Inside a string; `key` when it is an object key.
    String {
        key: bool,
    },
    /// After a backslash.
    Escape {
        key: bool,
    },
    /// Inside `\uXXXX`: the digits read so far.
    Unicode {
        key: bool,
        digits: u8,
        value: u16,
    },
    Number,
    Literal,
}

/// The streaming reader of one record. `select(path)` says whether the value at `path` is kept.
pub struct TranscriptJsonReader<'a> {
    select: &'a dyn Fn(&[PathSegment]) -> bool,
    allowance: usize,
    charged: usize,
    max_depth: usize,
    // Incremental UTF-8 decoding.
    pending: Vec<u8>,
    // Tokenizer.
    expect: Expect,
    lexeme: Lexeme,
    text: String,
    /// The UTF-16 length of the current string chunk (since the last write boundary).
    chunk_units: usize,
    /// A high surrogate waiting for its pair.
    high_surrogate: Option<u16>,
    selected_scalar: bool,
    depth: usize,
    complete: bool,
    malformed: bool,
    // Assembly.
    stack: Vec<Frame>,
    root: Option<Value>,
}

impl<'a> TranscriptJsonReader<'a> {
    /// A reader that may charge up to `allowance` bytes.
    pub fn new(allowance: usize, select: &'a dyn Fn(&[PathSegment]) -> bool) -> Self {
        Self::with_max_depth(allowance, select, DEFAULT_MAX_DEPTH)
    }

    pub fn with_max_depth(allowance: usize, select: &'a dyn Fn(&[PathSegment]) -> bool, max_depth: usize) -> Self {
        Self {
            select,
            allowance,
            charged: 0,
            max_depth,
            pending: Vec::new(),
            expect: Expect::Value { or_close: false },
            lexeme: Lexeme::None,
            text: String::new(),
            chunk_units: 0,
            high_surrogate: None,
            selected_scalar: false,
            depth: 0,
            complete: false,
            malformed: false,
            stack: Vec::new(),
            root: None,
        }
    }

    /// What this record has charged so far.
    pub fn charged(&self) -> usize {
        self.charged
    }

    fn reserve(&mut self, bytes: usize) -> Result<(), TranscriptJsonLimitError> {
        self.charged = self.charged.saturating_add(bytes);
        if self.charged > self.allowance {
            return Err(TranscriptJsonLimitError("Transcript selected history exceeds the 32 MiB memory budget".into()));
        }
        Ok(())
    }

    /// Feed raw bytes (any split, including inside a UTF-8 sequence).
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), TranscriptJsonLimitError> {
        if self.malformed {
            return Ok(());
        }
        self.pending.extend_from_slice(bytes);
        let pending = std::mem::take(&mut self.pending);
        let mut rest: &[u8] = &pending;
        loop {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.feed(text)?;
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // SAFETY-free: the prefix is valid UTF-8 by construction.
                    let text = std::str::from_utf8(&rest[..valid]).unwrap_or_default();
                    self.feed(text)?;
                    match error.error_len() {
                        Some(len) => {
                            self.feed("\u{FFFD}")?;
                            rest = &rest[valid + len..];
                        }
                        None => {
                            // An incomplete sequence at the end: wait for the next write.
                            self.pending = rest[valid..].to_vec();
                            break;
                        }
                    }
                }
            }
        }
        self.end_of_write()
    }

    /// End of input: the projected record, or `None` when it is malformed or incomplete.
    pub fn finish(mut self) -> Result<Option<Value>, TranscriptJsonLimitError> {
        if !self.pending.is_empty() && !self.malformed {
            let pending = std::mem::take(&mut self.pending);
            let text = String::from_utf8_lossy(&pending).into_owned();
            self.feed(&text)?;
        }
        if self.malformed {
            return Ok(None);
        }
        // A number or literal can end at the end of input.
        match self.lexeme {
            Lexeme::Number => self.end_number()?,
            Lexeme::Literal => self.end_literal()?,
            Lexeme::None => {}
            _ => return Ok(None),
        }
        if self.malformed || !self.complete || self.expect != Expect::Done {
            return Ok(None);
        }
        Ok(self.root.take())
    }

    fn fail(&mut self) {
        self.malformed = true;
    }

    fn end_of_write(&mut self) -> Result<(), TranscriptJsonLimitError> {
        // A string spanning writes is charged one chunk per write.
        if matches!(
            self.lexeme,
            Lexeme::String { key: false } | Lexeme::Escape { key: false } | Lexeme::Unicode { key: false, .. }
        ) && self.chunk_units > 0
        {
            if self.selected_scalar {
                self.reserve(64 + self.chunk_units * 2)?;
            }
            self.chunk_units = 0;
        }
        Ok(())
    }

    fn feed(&mut self, text: &str) -> Result<(), TranscriptJsonLimitError> {
        for c in text.chars() {
            if self.malformed {
                return Ok(());
            }
            self.step(c)?;
        }
        Ok(())
    }

    fn push_text(&mut self, c: char, key: bool) -> Result<(), TranscriptJsonLimitError> {
        let units = c.len_utf16();
        if key {
            self.reserve(units * 2)?;
        } else {
            self.chunk_units += units;
        }
        if key || self.selected_scalar {
            self.text.push(c);
        }
        Ok(())
    }

    fn push_unit(&mut self, unit: u16, key: bool) -> Result<(), TranscriptJsonLimitError> {
        if let Some(high) = self.high_surrogate.take() {
            if (0xDC00..0xE000).contains(&unit) {
                let code = 0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                // Both halves were counted as UTF-16 units already: charge the second here.
                return self.push_text_units(char::from_u32(code).unwrap_or('\u{FFFD}'), key, 1);
            }
            if key || self.selected_scalar {
                self.text.push('\u{FFFD}');
            }
        }
        if (0xD800..0xDC00).contains(&unit) {
            self.high_surrogate = Some(unit);
            // Charged now; the character lands with its pair.
            if key {
                self.reserve(2)?;
            } else {
                self.chunk_units += 1;
            }
            return Ok(());
        }
        let c = char::from_u32(u32::from(unit)).unwrap_or('\u{FFFD}');
        self.push_text(c, key)
    }

    /// Push a character already partially charged: only `units` more UTF-16 units are counted.
    fn push_text_units(&mut self, c: char, key: bool, units: usize) -> Result<(), TranscriptJsonLimitError> {
        if key {
            self.reserve(units * 2)?;
        } else {
            self.chunk_units += units;
        }
        if key || self.selected_scalar {
            self.text.push(c);
        }
        Ok(())
    }

    fn flush_surrogate(&mut self, key: bool) {
        if self.high_surrogate.take().is_some() && (key || self.selected_scalar) {
            self.text.push('\u{FFFD}');
        }
    }

    fn step(&mut self, c: char) -> Result<(), TranscriptJsonLimitError> {
        match self.lexeme {
            Lexeme::String { key } => {
                match c {
                    '"' => {
                        self.flush_surrogate(key);
                        self.lexeme = Lexeme::None;
                        if key {
                            self.end_key();
                        } else {
                            self.end_string()?;
                        }
                    }
                    '\\' => self.lexeme = Lexeme::Escape { key },
                    c if (c as u32) < 0x20 => self.fail(),
                    c => {
                        self.flush_surrogate(key);
                        self.push_text(c, key)?;
                    }
                }
                return Ok(());
            }
            Lexeme::Escape { key } => {
                let decoded = match c {
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'u' => {
                        self.lexeme = Lexeme::Unicode { key, digits: 0, value: 0 };
                        return Ok(());
                    }
                    _ => {
                        self.fail();
                        return Ok(());
                    }
                };
                self.flush_surrogate(key);
                self.lexeme = Lexeme::String { key };
                return self.push_text(decoded, key);
            }
            Lexeme::Unicode { key, digits, value } => {
                let Some(digit) = c.to_digit(16) else {
                    self.fail();
                    return Ok(());
                };
                let value = (value << 4) | digit as u16;
                if digits + 1 < 4 {
                    self.lexeme = Lexeme::Unicode {
                        key,
                        digits: digits + 1,
                        value,
                    };
                    return Ok(());
                }
                self.lexeme = Lexeme::String { key };
                return self.push_unit(value, key);
            }
            Lexeme::Number => {
                if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-') {
                    self.chunk_units += 1;
                    self.text.push(c);
                    return Ok(());
                }
                self.end_number()?;
                if self.malformed {
                    return Ok(());
                }
            }
            Lexeme::Literal => {
                if c.is_ascii_alphabetic() {
                    self.text.push(c);
                    return Ok(());
                }
                self.end_literal()?;
                if self.malformed {
                    return Ok(());
                }
            }
            Lexeme::None => {}
        }
        self.structural(c)
    }

    fn structural(&mut self, c: char) -> Result<(), TranscriptJsonLimitError> {
        if matches!(c, ' ' | '\t' | '\n' | '\r') {
            return Ok(());
        }
        match self.expect {
            Expect::Done => self.fail(),
            Expect::Colon => {
                if c == ':' {
                    self.expect = Expect::Value { or_close: false };
                } else {
                    self.fail();
                }
            }
            Expect::Key { or_close } => match c {
                '"' => {
                    self.text.clear();
                    self.lexeme = Lexeme::String { key: true };
                }
                '}' if or_close => self.end_container(false)?,
                _ => self.fail(),
            },
            Expect::CommaOrClose => {
                let is_array = self.stack.last().is_some_and(|frame| frame.is_array);
                match c {
                    ',' => {
                        self.expect = if is_array {
                            Expect::Value { or_close: false }
                        } else {
                            Expect::Key { or_close: false }
                        };
                    }
                    ']' if is_array => self.end_container(true)?,
                    '}' if !is_array => self.end_container(false)?,
                    _ => self.fail(),
                }
            }
            Expect::Value { or_close } => match c {
                ']' if or_close => self.end_container(true)?,
                '{' | '[' => {
                    self.depth += 1;
                    if self.depth > self.max_depth {
                        return Err(TranscriptJsonLimitError("Transcript JSON nesting exceeds the depth limit".into()));
                    }
                    let (path, selected) = self.start_value()?;
                    let is_array = c == '[';
                    if selected {
                        self.reserve(64)?;
                    }
                    self.stack.push(Frame {
                        path,
                        selected,
                        is_array,
                        key: None,
                        index: 0,
                        builder: selected.then(|| if is_array { Builder::Array(Vec::new()) } else { Builder::Object(Map::new()) }),
                    });
                    self.expect = if is_array {
                        Expect::Value { or_close: true }
                    } else {
                        Expect::Key { or_close: true }
                    };
                }
                '"' => {
                    let (_, selected) = self.start_value()?;
                    self.selected_scalar = selected;
                    if selected {
                        self.reserve(64)?;
                    }
                    self.text.clear();
                    self.chunk_units = 0;
                    self.lexeme = Lexeme::String { key: false };
                }
                '-' | '0'..='9' => {
                    let (_, selected) = self.start_value()?;
                    self.selected_scalar = selected;
                    if selected {
                        self.reserve(64)?;
                    }
                    self.text.clear();
                    self.text.push(c);
                    self.chunk_units = 1;
                    self.lexeme = Lexeme::Number;
                }
                't' | 'f' | 'n' => {
                    self.text.clear();
                    self.text.push(c);
                    self.lexeme = Lexeme::Literal;
                }
                _ => self.fail(),
            },
        }
        Ok(())
    }

    /// `startValue`: the path of the value starting now and whether it is selected (the
    /// selected key of an object member is charged here).
    fn start_value(&mut self) -> Result<(Vec<PathSegment>, bool), TranscriptJsonLimitError> {
        let Some(parent) = self.stack.last() else {
            let selected = (self.select)(&[]);
            return Ok((Vec::new(), selected));
        };
        if !parent.selected {
            return Ok((Vec::new(), false));
        }
        let segment = if parent.is_array {
            PathSegment::Index(parent.index)
        } else {
            parent.key.clone().map_or(PathSegment::Null, PathSegment::Key)
        };
        let mut path = parent.path.clone();
        path.push(segment);
        let selected = (self.select)(&path);
        if selected {
            if let Some(key) = parent.key.as_ref().filter(|_| !parent.is_array) {
                let units = key.encode_utf16().count();
                self.reserve(64 + units * 2)?;
            }
        }
        Ok((path, selected))
    }

    fn end_key(&mut self) {
        let key = std::mem::take(&mut self.text);
        if let Some(frame) = self.stack.last_mut() {
            frame.key = Some(key);
        }
        self.expect = Expect::Colon;
    }

    /// A finished value: into its parent (or the root), then advance the parent.
    fn add_value(&mut self, value: Option<Value>) {
        match self.stack.last_mut() {
            None => {
                self.root = value;
                self.expect = Expect::Done;
            }
            Some(frame) => {
                if let Some(value) = value {
                    match frame.builder.as_mut() {
                        Some(Builder::Array(items)) => items.push(value),
                        Some(Builder::Object(map)) => {
                            map.insert(frame.key.clone().unwrap_or_default(), value);
                        }
                        None => {}
                    }
                }
                if frame.is_array {
                    frame.index += 1;
                }
                self.expect = Expect::CommaOrClose;
            }
        }
    }

    fn end_string(&mut self) -> Result<(), TranscriptJsonLimitError> {
        let selected = self.selected_scalar;
        if selected {
            if self.chunk_units > 0 {
                self.reserve(64 + self.chunk_units * 2)?;
            }
            self.reserve(64)?;
        }
        self.chunk_units = 0;
        let value = selected.then(|| Value::String(std::mem::take(&mut self.text)));
        self.text.clear();
        self.add_value(value);
        Ok(())
    }

    fn end_number(&mut self) -> Result<(), TranscriptJsonLimitError> {
        self.lexeme = Lexeme::None;
        let text = std::mem::take(&mut self.text);
        if !number_is_valid(&text) {
            self.fail();
            return Ok(());
        }
        let selected = self.selected_scalar;
        if selected {
            self.reserve(64 + self.chunk_units * 2)?;
            self.reserve(64)?;
        }
        self.chunk_units = 0;
        let value = if selected {
            let parsed = serde_json::from_str::<Number>(&text)
                .ok()
                .or_else(|| text.parse::<f64>().ok().and_then(Number::from_f64));
            Some(parsed.map_or(Value::Null, Value::Number))
        } else {
            None
        };
        self.add_value(value);
        Ok(())
    }

    fn end_literal(&mut self) -> Result<(), TranscriptJsonLimitError> {
        self.lexeme = Lexeme::None;
        let text = std::mem::take(&mut self.text);
        let literal = match text.as_str() {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            "null" => Value::Null,
            _ => {
                self.fail();
                return Ok(());
            }
        };
        let (_, selected) = self.start_value()?;
        if selected {
            self.reserve(64)?;
        }
        self.add_value(selected.then_some(literal));
        Ok(())
    }

    fn end_container(&mut self, is_array: bool) -> Result<(), TranscriptJsonLimitError> {
        let Some(frame) = self.stack.pop() else {
            self.fail();
            return Ok(());
        };
        if frame.is_array != is_array {
            self.fail();
            return Ok(());
        }
        if frame.selected {
            self.reserve(64)?;
        }
        self.depth -= 1;
        if self.depth == 0 {
            self.complete = true;
        }
        let value = frame.builder.map(|builder| match builder {
            Builder::Object(map) => Value::Object(map),
            Builder::Array(items) => Value::Array(items),
        });
        self.add_value(value);
        Ok(())
    }
}

/// The JSON number grammar (`-?(0|[1-9]\d*)(\.\d+)?([eE][-+]?\d+)?`).
fn number_is_valid(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    i == bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(json: &str, size: usize, select: &dyn Fn(&[PathSegment]) -> bool) -> Option<Value> {
        let mut reader = TranscriptJsonReader::new(usize::MAX, select);
        let bytes = json.as_bytes();
        for chunk in bytes.chunks(size) {
            reader.write(chunk).unwrap();
        }
        reader.finish().unwrap()
    }

    fn all(_: &[PathSegment]) -> bool {
        true
    }

    #[test]
    fn matches_json_parse_across_boundaries() {
        for size in [1, 2, 7, 64, 1024] {
            for text in [
                r#"{"a":1,"a":2,"b":"before","b":"after"}"#,
                r#"{"a":{"x":1},"a":{"y":2},"b":[],"c":{}}"#,
                r#"{"a":[null,true,false,1,-2.3e4,"😀a\\\"",{},[],[1,2]]}"#,
                r#"{"a":"s","a":null,"b":null,"b":"s","c":0,"c":false}"#,
                r#"{"__proto__":{"polluted":true},"constructor":1,"__proto__":2}"#,
                r#"{"s":"😀 and é","t":"tab\there"}"#,
            ] {
                let expected: Value = serde_json::from_str(text).unwrap();
                assert_eq!(read(text, size, &all), Some(expected), "{text} at {size}");
            }
        }
    }

    #[test]
    fn projects_siblings_and_array_elements_without_merging_repeated_parents() {
        let text = r#"{"message":{"usage":{"input":100},"content":"large"},"message":{"usage":{"output":5},"content":[1,2]},"rows":[{"keep":1,"drop":2},{"keep":3}],"drop":{"keep":4}}"#;
        let select = |path: &[PathSegment]| {
            if path.first() == Some(&PathSegment::Key("drop".into())) {
                return false;
            }
            !path
                .iter()
                .any(|segment| matches!(segment, PathSegment::Key(key) if key == "content" || key == "drop"))
        };
        assert_eq!(
            read(text, 1, &select),
            Some(serde_json::json!({"message": {"usage": {"output": 5}}, "rows": [{"keep": 1}, {"keep": 3}]}))
        );
    }

    #[test]
    fn rejects_malformed_input() {
        for text in [
            r#"{"a":"#,
            r#"{"a":1} trailing"#,
            r#"{"a":1}{"a":2}"#,
            r#"{"a":"bad\x"}"#,
            r#"{"a":[1,]}"#,
            "5",
            "\"s\"",
            "",
        ] {
            assert_eq!(read(text, 1, &all), None, "{text}");
        }
    }

    #[test]
    fn retains_the_allocation_and_depth_limits() {
        let mut limited = TranscriptJsonReader::new(0, &all);
        assert!(limited.write(br#"{"a":1}"#).is_err());
        let deep = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        let mut reader = TranscriptJsonReader::new(usize::MAX, &all);
        let mut error = None;
        for chunk in deep.as_bytes().chunks(10) {
            if let Err(e) = reader.write(chunk) {
                error = Some(e);
                break;
            }
        }
        assert!(error.is_some());
        let ok = format!("{}0{}", "[".repeat(128), "]".repeat(128));
        assert!(read(&ok, 10, &all).is_some());
    }

    #[test]
    fn decodes_utf8_split_across_writes() {
        let text = r#"{"a":"héllo 😀"}"#;
        assert_eq!(read(text, 1, &all), Some(serde_json::json!({"a": "héllo 😀"})));
    }
}
