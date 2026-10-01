//! `Schema.Defect()` values: the `cause` field of tagged errors.
//!
//! Effect encodes an `Error` defect as `{"name": …, "message": …}` and passes any other value
//! through. Rust errors become the `Error` shape; a [`Defect`] can also carry an arbitrary JSON
//! value (for causes received from elsewhere, e.g. a decoded TS error).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// An encoded defect / error cause.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Defect(pub Value);

impl Defect {
    /// `{"name": name, "message": message}`, how Effect encodes an `Error` instance.
    pub fn error(name: &str, message: impl Into<String>) -> Self {
        Self(json!({ "name": name, "message": message.into() }))
    }

    /// An `Error` defect from any Rust error (`name` is `"Error"`).
    pub fn from_error(error: &(dyn std::error::Error + 'static)) -> Self {
        Self::error("Error", error.to_string())
    }

    /// The `message` of an `Error`-shaped defect, or the JSON text of anything else.
    pub fn message(&self) -> String {
        match &self.0 {
            Value::Object(map) => map
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| self.0.to_string()),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

impl From<&std::io::Error> for Defect {
    fn from(error: &std::io::Error) -> Self {
        Self::error("Error", error.to_string())
    }
}

impl std::fmt::Display for Defect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// JS `String.prototype.length` of a Rust string: its UTF-16 code-unit count. Wire fields such as
/// `stderrLength` were computed in JS, so they must count code units, not bytes or chars.
pub fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_errors_like_effect() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "nope");
        let defect = Defect::from(&io);
        assert_eq!(serde_json::to_string(&defect).unwrap(), r#"{"name":"Error","message":"nope"}"#);
        assert_eq!(defect.message(), "nope");
        assert_eq!(Defect(json!("plain")).message(), "plain");
    }

    #[test]
    fn js_length_counts_utf16_code_units() {
        assert_eq!(js_length("abc"), 3);
        assert_eq!(js_length("é"), 1);
        assert_eq!(js_length("😀"), 2);
    }
}
