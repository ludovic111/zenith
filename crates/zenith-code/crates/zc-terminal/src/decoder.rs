//! Streaming UTF-8 decoding of PTY output, exactly like Node's `StringDecoder("utf8")`
//! (`src/string_decoder.cc`), which node-pty applies to the PTY socket
//! (`socket.setEncoding("utf8")`).
//!
//! A character cut by a read boundary is held back and completed by the next read; invalid
//! bytes become U+FFFD with the same "maximal subpart" rule V8 uses, so each output chunk is
//! the same string the TS server would have emitted for the same reads. Checked against
//! `fixtures/decoder.json`, generated with Node.

/// See the module docs.
#[derive(Debug, Default, Clone)]
pub struct Utf8StreamDecoder {
    buffer: [u8; 4],
    buffered: usize,
    missing: usize,
}

impl Utf8StreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// `decoder.write(chunk)`.
    pub fn write(&mut self, chunk: &[u8]) -> String {
        let mut data = chunk;
        let mut prepend: Option<String> = None;

        if self.missing > 0 {
            // A non-continuation byte ends the incomplete character early; the bytes before
            // it still complete what can be completed.
            for i in 0..data.len().min(self.missing) {
                if data[i] & 0xc0 != 0x80 {
                    self.missing = 0;
                    self.buffer[self.buffered..self.buffered + i].copy_from_slice(&data[..i]);
                    self.buffered += i;
                    data = &data[i..];
                    break;
                }
            }
            let found = data.len().min(self.missing);
            self.buffer[self.buffered..self.buffered + found].copy_from_slice(&data[..found]);
            data = &data[found..];
            self.missing -= found;
            self.buffered += found;
            if self.missing == 0 {
                prepend = Some(String::from_utf8_lossy(&self.buffer[..self.buffered]).into_owned());
                self.buffered = 0;
            }
        }

        if data.is_empty() {
            return prepend.unwrap_or_default();
        }

        if data[data.len() - 1] & 0x80 != 0 {
            // Find where the last character starts, to hold it back if it is incomplete.
            let mut i = data.len() - 1;
            loop {
                self.buffered += 1;
                if data[i] & 0xc0 == 0x80 {
                    if self.buffered >= 4 || i == 0 {
                        self.buffered = 0;
                        break;
                    }
                } else {
                    let length = if data[i] & 0xe0 == 0xc0 {
                        2
                    } else if data[i] & 0xf0 == 0xe0 {
                        3
                    } else if data[i] & 0xf8 == 0xf0 {
                        4
                    } else {
                        self.buffered = 0;
                        break;
                    };
                    if self.buffered >= length {
                        self.missing = 0;
                        self.buffered = 0;
                    } else {
                        self.missing = length - self.buffered;
                    }
                    break;
                }
                i -= 1;
            }
        }

        let keep = data.len() - self.buffered;
        if self.buffered > 0 {
            self.buffer[..self.buffered].copy_from_slice(&data[keep..]);
        }
        let body = String::from_utf8_lossy(&data[..keep]);
        match prepend {
            Some(mut text) => {
                text.push_str(&body);
                text
            }
            None => body.into_owned(),
        }
    }

    /// `decoder.end()`: what an incomplete trailing character becomes at the end of input.
    pub fn end(&mut self) -> String {
        let rest = String::from_utf8_lossy(&self.buffer[..self.buffered]).into_owned();
        self.buffered = 0;
        self.missing = 0;
        rest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_back_split_characters() {
        let bytes = "é🚀".as_bytes();
        let mut decoder = Utf8StreamDecoder::new();
        let mut out = String::new();
        for byte in bytes {
            out.push_str(&decoder.write(&[*byte]));
        }
        assert_eq!(out, "é🚀");
        assert_eq!(decoder.write(&[0xe5, 0x90]), "");
        assert_eq!(decoder.write(b"a"), "\u{fffd}a");
    }
}
