//! Tool parameter decoding, as Effect's `Schema.decodeUnknownEffect(tool.parametersSchema)`
//! does it for the toolkits: fields in schema order (the decoded object keeps that order and
//! drops unknown keys), `TrimmedNonEmptyString` trims before its checks, `optional` rejects
//! `null`, the first failure wins, and the message reads like Effect's
//! (`Expected a value with a length of at least 1\n  at ["url"]`). Cross-field filters run on
//! the decoded object and fail with their own message, without a path.

use serde_json::{Map, Number, Value};

/// A JS string's length (UTF-16 code units).
pub fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `String.prototype.trim`: Unicode white space plus the BOM.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// A numeric bound.
#[derive(Debug, Clone, Copy)]
pub enum Bound {
    /// `>= n` / `<= n`.
    Inclusive(f64),
    /// `> n` / `< n`.
    Exclusive(f64),
}

/// A parameter schema.
#[derive(Debug, Clone)]
pub enum Spec {
    /// `Schema.String`.
    String,
    /// `TrimmedNonEmptyString` (+ `isMaxLength`): trimmed, then non-empty.
    Trimmed {
        max: Option<usize>,
    },
    /// `Schema.String.check(isTrimmed()).check(isNonEmpty())` (+ `isMaxLength`): rejected, not
    /// trimmed, when it has surrounding white space.
    CheckedTrimmed {
        max: Option<usize>,
    },
    Boolean,
    /// `Schema.Finite` / `Schema.Number`.
    Number,
    /// `Schema.Int` with bounds; `between` reports both bounds at once.
    Int {
        min: Option<Bound>,
        max: Option<Bound>,
        between: bool,
    },
    Literal(&'static [&'static str]),
    Array(Box<Spec>),
    /// `Schema.Unknown`.
    Unknown,
    Struct(Vec<Field>),
    /// A struct union told apart by a literal `key`; `expected` is Effect's rendering of the
    /// members for a value no member takes.
    Tagged {
        key: &'static str,
        members: Vec<(&'static str, Spec)>,
        expected: &'static str,
    },
}

/// One struct field.
#[derive(Debug, Clone)]
pub struct Field {
    pub key: &'static str,
    pub spec: Spec,
    pub optional: bool,
}

pub fn required(key: &'static str, spec: Spec) -> Field {
    Field { key, spec, optional: false }
}

pub fn optional(key: &'static str, spec: Spec) -> Field {
    Field { key, spec, optional: true }
}

/// A decode failure: Effect's message and path.
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    pub message: String,
    pub path: Vec<PathSegment>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PathSegment {
    Key(String),
    Index(usize),
}

impl Issue {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            path: Vec::new(),
        }
    }

    fn at(mut self, segment: PathSegment) -> Self {
        self.path.insert(0, segment);
        self
    }

    /// `message\n  at ["a"][0]`, or the message alone without a path.
    pub fn render(&self) -> String {
        if self.path.is_empty() {
            return self.message.clone();
        }
        let path: String = self
            .path
            .iter()
            .map(|segment| match segment {
                PathSegment::Key(key) => format!("[{}]", serde_json::to_string(key).unwrap_or_default()),
                PathSegment::Index(index) => format!("[{index}]"),
            })
            .collect();
        format!("{}\n  at {path}", self.message)
    }
}

fn type_name(spec: &Spec) -> String {
    match spec {
        Spec::String | Spec::Trimmed { .. } | Spec::CheckedTrimmed { .. } => "string".into(),
        Spec::Boolean => "boolean".into(),
        Spec::Number | Spec::Int { .. } => "number".into(),
        Spec::Literal(options) => options.iter().map(|option| format!("\"{option}\"")).collect::<Vec<_>>().join(" | "),
        Spec::Array(_) => "array".into(),
        Spec::Unknown => "unknown".into(),
        Spec::Struct(_) => "object".into(),
        Spec::Tagged { expected, .. } => (*expected).into(),
    }
}

/// Whether `value` is of the spec's base JS type (what Effect's union pre-filter checks).
fn has_base_type(spec: &Spec, value: &Value) -> bool {
    match spec {
        Spec::String | Spec::Trimmed { .. } | Spec::CheckedTrimmed { .. } | Spec::Literal(_) => value.is_string(),
        Spec::Boolean => value.is_boolean(),
        Spec::Number | Spec::Int { .. } => value.is_number(),
        Spec::Array(_) => value.is_array(),
        Spec::Unknown => true,
        Spec::Struct(_) | Spec::Tagged { .. } => value.is_object(),
    }
}

fn format_bound(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

fn check_length(text: &str, max: Option<usize>) -> Result<(), Issue> {
    if js_length(text) < 1 {
        return Err(Issue::new("Expected a value with a length of at least 1"));
    }
    if let Some(max) = max {
        if js_length(text) > max {
            return Err(Issue::new(format!("Expected a value with a length of at most {max}")));
        }
    }
    Ok(())
}

/// Decodes `value` against `spec` (the value is present and not `undefined`).
pub fn decode(spec: &Spec, value: &Value) -> Result<Value, Issue> {
    match spec {
        Spec::Unknown => Ok(value.clone()),
        Spec::String => match value {
            Value::String(_) => Ok(value.clone()),
            _ => Err(Issue::new("Expected string")),
        },
        Spec::Trimmed { max } => {
            let Value::String(text) = value else {
                return Err(Issue::new("Expected string"));
            };
            let trimmed = js_trim(text);
            check_length(trimmed, *max)?;
            Ok(Value::String(trimmed.to_owned()))
        }
        Spec::CheckedTrimmed { max } => {
            let Value::String(text) = value else {
                return Err(Issue::new("Expected string"));
            };
            if js_trim(text) != text {
                return Err(Issue::new("Expected a string with no leading or trailing whitespace"));
            }
            check_length(text, *max)?;
            Ok(value.clone())
        }
        Spec::Boolean => match value {
            Value::Bool(_) => Ok(value.clone()),
            _ => Err(Issue::new("Expected boolean")),
        },
        Spec::Number => match value {
            Value::Number(_) => Ok(value.clone()),
            _ => Err(Issue::new("Expected number")),
        },
        Spec::Int { min, max, between } => {
            let Some(number) = value.as_f64().filter(|_| value.is_number()) else {
                return Err(Issue::new("Expected number"));
            };
            if number.fract() != 0.0 {
                return Err(Issue::new("Expected an integer"));
            }
            if *between {
                if let (Some(Bound::Inclusive(low)), Some(Bound::Inclusive(high))) = (min, max) {
                    if number < *low || number > *high {
                        return Err(Issue::new(format!(
                            "Expected a value between {} and {}",
                            format_bound(*low),
                            format_bound(*high)
                        )));
                    }
                }
            } else {
                match min {
                    Some(Bound::Inclusive(low)) if number < *low => {
                        return Err(Issue::new(format!("Expected a value greater than or equal to {}", format_bound(*low))));
                    }
                    Some(Bound::Exclusive(low)) if number <= *low => {
                        return Err(Issue::new(format!("Expected a value greater than {}", format_bound(*low))));
                    }
                    _ => {}
                }
                match max {
                    Some(Bound::Inclusive(high)) if number > *high => {
                        return Err(Issue::new(format!("Expected a value less than or equal to {}", format_bound(*high))));
                    }
                    Some(Bound::Exclusive(high)) if number >= *high => {
                        return Err(Issue::new(format!("Expected a value less than {}", format_bound(*high))));
                    }
                    _ => {}
                }
            }
            // Keep integers integral on the wire (`2`, not `2.0`).
            Ok(match value.as_i64() {
                Some(_) => value.clone(),
                None => Number::from_f64(number)
                    .map(|n| match n.as_f64() {
                        Some(f) if f.fract() == 0.0 && f.abs() < 9.0e15 => Value::from(f as i64),
                        _ => Value::Number(n),
                    })
                    .unwrap_or_else(|| value.clone()),
            })
        }
        Spec::Literal(options) => match value {
            Value::String(text) if options.contains(&text.as_str()) => Ok(value.clone()),
            _ => Err(Issue::new(format!("Expected {}", type_name(spec)))),
        },
        Spec::Array(item) => {
            let Value::Array(items) = value else {
                return Err(Issue::new("Expected array"));
            };
            items
                .iter()
                .enumerate()
                .map(|(index, element)| decode(item, element).map_err(|issue| issue.at(PathSegment::Index(index))))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array)
        }
        Spec::Struct(fields) => {
            let Value::Object(object) = value else {
                return Err(Issue::new("Expected object"));
            };
            decode_struct(fields, object, false).map(Value::Object)
        }
        Spec::Tagged { key, members, .. } => {
            let Value::Object(object) = value else {
                return Err(Issue::new(format!("Expected {}", type_name(spec))));
            };
            let tag = object.get(*key).and_then(Value::as_str);
            match members.iter().find(|(name, _)| Some(*name) == tag) {
                Some((_, member)) => decode(member, value),
                None => Err(Issue::new(format!("Expected {}", type_name(spec)))),
            }
        }
    }
}

/// Decodes a struct's fields in order; `closed` rejects every key (`Expected never`), as the
/// empty parameter struct of a tool without parameters does.
pub fn decode_struct(fields: &[Field], object: &Map<String, Value>, closed: bool) -> Result<Map<String, Value>, Issue> {
    let mut out = Map::new();
    for field in fields {
        let Some(value) = object.get(field.key) else {
            if field.optional {
                continue;
            }
            return Err(Issue::new("Missing key").at(PathSegment::Key(field.key.into())));
        };
        let decoded = if field.optional && !has_base_type(&field.spec, value) {
            Err(Issue::new(format!("Expected {} | undefined", type_name(&field.spec))))
        } else {
            decode(&field.spec, value)
        };
        out.insert(field.key.into(), decoded.map_err(|issue| issue.at(PathSegment::Key(field.key.into())))?);
    }
    if closed {
        if let Some(key) = object.keys().find(|key| !fields.iter().any(|field| field.key == key.as_str())) {
            return Err(Issue::new("Expected never").at(PathSegment::Key(key.clone())));
        }
    }
    Ok(out)
}

/// Decodes a tool's arguments object (`payload ?? {}`).
pub fn decode_arguments(fields: &[Field], arguments: Option<&Value>, closed: bool) -> Result<Map<String, Value>, Issue> {
    match arguments {
        None => decode_struct(fields, &Map::new(), closed),
        Some(Value::Object(object)) => decode_struct(fields, object, closed),
        Some(_) => Err(Issue::new("Expected object")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn int(min: Option<Bound>, max: Option<Bound>) -> Spec {
        Spec::Int { min, max, between: false }
    }

    #[test]
    fn renders_paths_like_effect() {
        let fields = vec![
            optional("tabId", Spec::Trimmed { max: Some(128) }),
            required("key", Spec::CheckedTrimmed { max: None }),
        ];
        let render = |value: Value| decode_arguments(&fields, Some(&value), false).map_err(|issue| issue.render());
        assert_eq!(
            render(json!({"tabId": 3, "key": "a"})),
            Err("Expected string | undefined\n  at [\"tabId\"]".into())
        );
        assert_eq!(
            render(json!({"tabId": null, "key": "a"})),
            Err("Expected string | undefined\n  at [\"tabId\"]".into())
        );
        assert_eq!(render(json!({})), Err("Missing key\n  at [\"key\"]".into()));
        assert_eq!(render(json!({"key": 3})), Err("Expected string\n  at [\"key\"]".into()));
        assert_eq!(
            render(json!({"key": " a"})),
            Err("Expected a string with no leading or trailing whitespace\n  at [\"key\"]".into())
        );
        assert_eq!(
            render(json!({"tabId": " "})),
            Err("Expected a value with a length of at least 1\n  at [\"tabId\"]".into())
        );
        assert_eq!(
            render(json!({"key": "a", "tabId": " t ", "extra": 1})),
            Ok(json!({"tabId": "t", "key": "a"}).as_object().unwrap().clone())
        );
        assert_eq!(decode_arguments(&fields, Some(&json!([])), false).unwrap_err().render(), "Expected object");
    }

    #[test]
    fn checks_numbers_literals_and_arrays() {
        let fields = vec![
            optional("timeoutMs", int(Some(Bound::Exclusive(0.0)), Some(Bound::Inclusive(60_000.0)))),
            optional("port", int(Some(Bound::Exclusive(0.0)), Some(Bound::Exclusive(65_536.0)))),
            optional(
                "width",
                Spec::Int {
                    min: Some(Bound::Inclusive(240.0)),
                    max: Some(Bound::Inclusive(3840.0)),
                    between: true,
                },
            ),
            optional("modifiers", Spec::Array(Box::new(Spec::Literal(&["Alt", "Meta"])))),
            optional("mode", Spec::Literal(&["fill", "freeform"])),
        ];
        let render = |value: Value| decode_arguments(&fields, Some(&value), false).map_err(|issue| issue.render());
        assert_eq!(
            render(json!({"timeoutMs": 0})),
            Err("Expected a value greater than 0\n  at [\"timeoutMs\"]".into())
        );
        assert_eq!(
            render(json!({"timeoutMs": 60001})),
            Err("Expected a value less than or equal to 60000\n  at [\"timeoutMs\"]".into())
        );
        assert_eq!(render(json!({"timeoutMs": 1.5})), Err("Expected an integer\n  at [\"timeoutMs\"]".into()));
        assert_eq!(render(json!({"port": 70000})), Err("Expected a value less than 65536\n  at [\"port\"]".into()));
        assert_eq!(
            render(json!({"width": 100})),
            Err("Expected a value between 240 and 3840\n  at [\"width\"]".into())
        );
        assert_eq!(
            render(json!({"modifiers": ["Hyper"]})),
            Err("Expected \"Alt\" | \"Meta\"\n  at [\"modifiers\"][0]".into())
        );
        assert_eq!(
            render(json!({"modifiers": "Meta"})),
            Err("Expected array | undefined\n  at [\"modifiers\"]".into())
        );
        assert_eq!(render(json!({"mode": "big"})), Err("Expected \"fill\" | \"freeform\"\n  at [\"mode\"]".into()));
        assert_eq!(render(json!({"timeoutMs": 2000.0})).unwrap()["timeoutMs"].to_string(), "2000");
    }

    #[test]
    fn rejects_every_key_of_a_closed_struct() {
        assert_eq!(
            decode_arguments(&[], Some(&json!({"x": 1})), true).unwrap_err().render(),
            "Expected never\n  at [\"x\"]"
        );
        assert!(decode_arguments(&[], None, true).unwrap().is_empty());
    }
}
