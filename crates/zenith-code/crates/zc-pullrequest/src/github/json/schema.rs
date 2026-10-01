//! What `decodeJsonResult(schema)` and `Schema.decodeUnknownExit(schema)` do, for the Effect
//! schemas of `gitHubPullRequestJson.ts`: `JSON.parse`, then a structural check with the same
//! strictness. A `Schema.Struct` takes a plain object (never an array or `null`) and ignores keys
//! it does not declare; `Schema.optional(X)` lets a key be absent but not `null`;
//! `Schema.NullOr(X)` lets it be `null`; `Schema.Int` is a safe integer.
//!
//! The raw schemas of the TS become small `raw_*` functions over [`serde_json::Value`] built from
//! these helpers, so a value the TS rejects is rejected here too, and the failure says where, the
//! way `formatSchemaError` (`@t3tools/shared/schemaJson`) does.

use std::fmt;

use serde_json::{Map, Value};
use zc_sourcecontrol::errors::{error_defect, CauseError};
use zc_sourcecontrol::Cause;

/// One step of the path to a failing value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

/// `DecodeFailure` (`Cause.Cause<Schema.SchemaError>`): why a `gh` answer did not decode. The
/// TS keeps the schema `Cause`; this keeps what `formatSchemaError` reads off it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeFailure {
    /// `"Invalid type"`, `"Missing key"` or `"Invalid value"` (a failed check, a union no member
    /// matched, or text that is not JSON).
    pub issue: &'static str,
    /// From the root to the failing value.
    pub path: Vec<PathSegment>,
}

const MAX_PATH_SEGMENTS: usize = 16;
const MAX_PATH_SEGMENT_LENGTH: usize = 64;

impl DecodeFailure {
    pub(crate) fn invalid_type() -> Self {
        Self {
            issue: "Invalid type",
            path: Vec::new(),
        }
    }

    pub(crate) fn missing_key() -> Self {
        Self {
            issue: "Missing key",
            path: Vec::new(),
        }
    }

    pub(crate) fn invalid_value() -> Self {
        Self {
            issue: "Invalid value",
            path: Vec::new(),
        }
    }

    fn at(mut self, segment: PathSegment) -> Self {
        self.path.insert(0, segment);
        self
    }

    /// `formatSchemaError`: the issue, then `\n  at ["key"][0]…` where there is a path.
    pub fn message(&self) -> String {
        if self.path.is_empty() {
            return self.issue.to_owned();
        }
        let mut path = String::new();
        for segment in self.path.iter().take(MAX_PATH_SEGMENTS) {
            match segment {
                PathSegment::Index(index) => path.push_str(&format!("[{index}]")),
                PathSegment::Key(key) => {
                    let units: Vec<u16> = key.encode_utf16().collect();
                    let key = if units.len() <= MAX_PATH_SEGMENT_LENGTH {
                        key.clone()
                    } else {
                        format!("{}...", String::from_utf16_lossy(&units[..MAX_PATH_SEGMENT_LENGTH - 3]))
                    };
                    path.push_str(&format!("[{}]", Value::String(key)));
                }
            }
        }
        if self.path.len() > MAX_PATH_SEGMENTS {
            path.push_str("[...]");
        }
        format!("{}\n  at {path}", self.issue)
    }

    /// The failure as a `cause`, encoded like the schema error it stands for.
    pub fn cause(&self) -> Cause {
        Cause::new(self.clone())
    }
}

impl fmt::Display for DecodeFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for DecodeFailure {}

impl CauseError for DecodeFailure {
    fn defect(&self) -> Value {
        error_defect("SchemaError", self.message(), None)
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl From<DecodeFailure> for Cause {
    fn from(failure: DecodeFailure) -> Self {
        Cause::new(failure)
    }
}

pub(crate) type Decoded<T> = Result<T, DecodeFailure>;

/// `JSON.parse`, as `Schema.fromJsonString` runs it.
pub(crate) fn parse_json(raw: &str) -> Decoded<Value> {
    serde_json::from_str(raw).map_err(|_| DecodeFailure::invalid_value())
}

/// `Schema.Struct`'s own check: a plain object.
pub(crate) fn object(value: &Value) -> Decoded<&Map<String, Value>> {
    value.as_object().ok_or_else(DecodeFailure::invalid_type)
}

/// `Schema.String`.
pub(crate) fn string(value: &Value) -> Decoded<String> {
    value.as_str().map(str::to_owned).ok_or_else(DecodeFailure::invalid_type)
}

/// `Schema.Number`: any JSON number.
pub(crate) fn number(value: &Value) -> Decoded<f64> {
    value.as_f64().ok_or_else(DecodeFailure::invalid_type)
}

/// `Schema.Int`: a number that is a safe integer (`1.0` is one, as `JSON.parse` reads it).
pub(crate) fn int(value: &Value) -> Decoded<i64> {
    const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    let Value::Number(number) = value else {
        return Err(DecodeFailure::invalid_type());
    };
    if let Some(integer) = number.as_i64() {
        return if integer.abs() <= MAX_SAFE_INTEGER {
            Ok(integer)
        } else {
            Err(DecodeFailure::invalid_value())
        };
    }
    match number.as_f64() {
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        Some(float) if float.fract() == 0.0 && float.abs() <= MAX_SAFE_INTEGER as f64 => Ok(float as i64),
        _ => Err(DecodeFailure::invalid_value()),
    }
}

/// `Schema.Boolean`.
pub(crate) fn boolean(value: &Value) -> Decoded<bool> {
    value.as_bool().ok_or_else(DecodeFailure::invalid_type)
}

/// `Schema.Unknown`.
pub(crate) fn unknown(value: &Value) -> Decoded<Value> {
    Ok(value.clone())
}

/// A union (`NullOr`, `optional`, `Union`) reports a value no member takes by its type as
/// `Invalid value`; a member that takes the type reports its own issue.
pub(crate) fn union_member<T>(decoded: Decoded<T>) -> Decoded<T> {
    decoded.map_err(|failure| {
        if failure.path.is_empty() && failure.issue == "Invalid type" {
            DecodeFailure::invalid_value()
        } else {
            failure
        }
    })
}

/// `Schema.NullOr(X)`.
pub(crate) fn null_or<T>(value: &Value, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Option<T>> {
    if value.is_null() {
        Ok(None)
    } else {
        union_member(decode(value)).map(Some)
    }
}

/// `Schema.Array(X)`.
pub(crate) fn array<T>(value: &Value, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Vec<T>> {
    let items = value.as_array().ok_or_else(DecodeFailure::invalid_type)?;
    items
        .iter()
        .enumerate()
        .map(|(index, item)| decode(item).map_err(|failure| failure.at(PathSegment::Index(index))))
        .collect()
}

/// `Schema.Record(Schema.String, X)`: every entry, in the order the answer gave them.
pub(crate) fn record<T>(value: &Value, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Vec<(String, T)>> {
    object(value)?
        .iter()
        .map(|(key, item)| {
            decode(item)
                .map(|decoded| (key.clone(), decoded))
                .map_err(|failure| failure.at(PathSegment::Key(key.clone())))
        })
        .collect()
}

/// A required property.
pub(crate) fn req<T>(map: &Map<String, Value>, key: &str, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<T> {
    match map.get(key) {
        None => Err(DecodeFailure::missing_key().at(PathSegment::Key(key.to_owned()))),
        Some(value) => decode(value).map_err(|failure| failure.at(PathSegment::Key(key.to_owned()))),
    }
}

/// `Schema.optional(X)`: absent is `None`, `null` fails unless `X` takes it.
pub(crate) fn opt<T>(map: &Map<String, Value>, key: &str, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Option<T>> {
    match map.get(key) {
        None => Ok(None),
        Some(value) => union_member(decode(value))
            .map(Some)
            .map_err(|failure| failure.at(PathSegment::Key(key.to_owned()))),
    }
}

/// `Schema.optional(Schema.NullOr(X))` where absent and `null` read the same.
pub(crate) fn opt_n<T>(map: &Map<String, Value>, key: &str, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Option<T>> {
    Ok(opt(map, key, |value| null_or(value, &decode))?.flatten())
}

/// A required `Schema.NullOr(X)`.
pub(crate) fn req_n<T>(map: &Map<String, Value>, key: &str, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Option<T>> {
    req(map, key, |value| null_or(value, &decode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_an_integral_float_as_an_int_and_refuses_a_fraction() {
        assert_eq!(int(&json!(1.0)), Ok(1));
        assert_eq!(int(&json!(1.5)), Err(DecodeFailure::invalid_value()));
        assert_eq!(int(&json!("1")), Err(DecodeFailure::invalid_type()));
        assert!(int(&json!(9_007_199_254_740_992_i64)).is_err());
    }

    #[test]
    fn formats_the_path_like_format_schema_error() {
        let failure = req(object(&json!({"data": [{}]})).unwrap(), "data", |value| {
            array(value, |item| req(object(item)?, "id", string))
        })
        .unwrap_err();
        assert_eq!(failure.message(), "Missing key\n  at [\"data\"][0][\"id\"]");
        assert_eq!(failure.cause().name().as_deref(), Some("SchemaError"));
    }

    #[test]
    fn tells_absent_from_null_only_where_asked() {
        let map = json!({"a": null});
        let map = object(&map).unwrap();
        assert!(opt(map, "a", string).is_err());
        assert_eq!(opt_n(map, "a", string), Ok(None));
        assert_eq!(opt(map, "b", |value| null_or(value, string)), Ok(None));
        assert_eq!(opt(map, "a", |value| null_or(value, string)), Ok(Some(None)));
    }
}
