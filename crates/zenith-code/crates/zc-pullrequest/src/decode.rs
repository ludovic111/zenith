//! Payload decoding of the `pullRequests.*` RPCs and `POST /api/pull-requests/diff`: the parts of
//! the Effect schemas (`packages/contracts/src/pullRequest.ts`) that serde does not do by itself.
//!
//! The generated zc-contracts types keep the base types only, so the transformations and checks
//! are applied here on the JSON before it is deserialized: `TrimmedNonEmptyString` (and the
//! branded ids built on it) is trimmed then refused when empty, `PositiveInt` refused below 1,
//! and the length bounds of `CommentBody`, `FilePath`, list cursors, qualifiers, titles and
//! batches are enforced. A payload that fails is a decode failure (`Die` on the RPC, a 400
//! `HttpApiDecodeError` over HTTP), like the TS schema decode.

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Why a payload does not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeIssue(pub String);

impl std::fmt::Display for DecodeIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Check = Result<(), DecodeIssue>;

fn issue(path: &str, message: impl std::fmt::Display) -> DecodeIssue {
    DecodeIssue(format!("{message}\n  at {path}"))
}

fn field_path(path: &str, key: &str) -> String {
    format!("{path}[\"{key}\"]")
}

/// JS `String.prototype.trim` (zc-sourcecontrol's `js_trim`).
fn trim(text: &str) -> &str {
    zc_sourcecontrol::util::js_trim(text)
}

/// JS string length (UTF-16 code units), which is what `isMaxLength` counts.
fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

fn object<'a>(value: &'a mut Value, path: &str) -> Result<&'a mut Map<String, Value>, DecodeIssue> {
    match value {
        Value::Object(map) => Ok(map),
        other => Err(issue(path, format!("Expected object, got {other}"))),
    }
}

/// `TrimmedNonEmptyString` (optionally bounded) at `key`, when present (`required` refuses an
/// absent key).
fn trimmed(map: &mut Map<String, Value>, path: &str, key: &str, required: bool, max: Option<usize>) -> Check {
    let at = field_path(path, key);
    match map.get_mut(key) {
        None => {
            if required {
                Err(issue(&at, "Missing key"))
            } else {
                Ok(())
            }
        }
        Some(value) => trimmed_value(value, &at, max),
    }
}

fn trimmed_value(value: &mut Value, at: &str, max: Option<usize>) -> Check {
    let Value::String(text) = value else {
        return Err(issue(at, format!("Expected string, got {value}")));
    };
    let next = trim(text).to_owned();
    if next.is_empty() {
        return Err(issue(at, "Expected a value with a length of at least 1, got \"\""));
    }
    if let Some(max) = max {
        if js_length(&next) > max {
            return Err(issue(at, format!("Expected a value with a length of at most {max}")));
        }
    }
    *text = next;
    Ok(())
}

/// A string that is not trimmed but must be non-empty and/or bounded.
fn bounded_string(map: &Map<String, Value>, path: &str, key: &str, non_empty: bool, max: usize) -> Check {
    let at = field_path(path, key);
    match map.get(key) {
        None => Ok(()),
        Some(Value::String(text)) => {
            if non_empty && text.is_empty() {
                Err(issue(&at, "Expected a value with a length of at least 1, got \"\""))
            } else if js_length(text) > max {
                Err(issue(&at, format!("Expected a value with a length of at most {max}")))
            } else {
                Ok(())
            }
        }
        Some(other) => Err(issue(&at, format!("Expected string, got {other}"))),
    }
}

/// `PositiveInt` at `key`, when present.
fn positive_int(map: &Map<String, Value>, path: &str, key: &str) -> Check {
    match map.get(key) {
        None => Ok(()),
        Some(value) => {
            let at = field_path(path, key);
            match value.as_f64() {
                Some(number) if number.fract() == 0.0 && number >= 1.0 => Ok(()),
                Some(number) if number.fract() != 0.0 => Err(issue(&at, format!("Expected an integer, got {number}"))),
                Some(number) => Err(issue(&at, format!("Expected a value greater than or equal to 1, got {number}"))),
                None => Err(issue(&at, format!("Expected number, got {value}"))),
            }
        }
    }
}

fn max_items(map: &Map<String, Value>, path: &str, key: &str, min: usize, max: usize) -> Check {
    match map.get(key) {
        Some(Value::Array(items)) => {
            let at = field_path(path, key);
            if items.len() < min {
                Err(issue(&at, format!("Expected a value with a length of at least {min}")))
            } else if items.len() > max {
                Err(issue(&at, format!("Expected a value with a length of at most {max}")))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

fn each_item(map: &mut Map<String, Value>, path: &str, key: &str, mut check: impl FnMut(&mut Value, &str) -> Check) -> Check {
    if let Some(Value::Array(items)) = map.get_mut(key) {
        let base = field_path(path, key);
        for (index, item) in items.iter_mut().enumerate() {
            check(item, &format!("{base}[{index}]"))?;
        }
    }
    Ok(())
}

/// The fields of `PullRequestRef` (every per-change-request input spreads them).
fn reference_fields(map: &mut Map<String, Value>, path: &str) -> Check {
    trimmed(map, path, "projectId", true, None)?;
    trimmed(map, path, "host", false, None)?;
    trimmed(map, path, "expectedAccountId", false, None)?;
    trimmed(map, path, "repository", true, None)?;
    positive_int(map, path, "number")
}

fn reference_value(value: &mut Value, path: &str) -> Check {
    reference_fields(object(value, path)?, path)
}

/// `CommentBody`: non-empty, at most 65,536.
const COMMENT_BODY_MAX: usize = 65_536;
/// `FilePath`: non-empty, at most 4,096, not trimmed.
const FILE_PATH_MAX: usize = 4_096;

/// The normalization of one method's payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadSchema {
    Ref,
    List,
    ListStats,
    RoutingIdentity,
    ThreadComments,
    Diff,
    DiffFileContents,
    SetFilesViewed,
    Action,
    Update,
    Comment,
    CommentUpdate,
    SubmitReview,
    ThreadReply,
    ThreadResolution,
    Reaction,
    ReviewerRequest,
    LabelChange,
    Invalidate,
    Empty,
}

/// Applies `schema`'s transformations and checks to `value` in place.
pub fn normalize(schema: PayloadSchema, value: &mut Value) -> Check {
    let path = "";
    if schema == PayloadSchema::Empty {
        object(value, path)?;
        return Ok(());
    }
    let map = object(value, path)?;
    match schema {
        PayloadSchema::Empty => {}
        PayloadSchema::Ref => reference_fields(map, path)?,
        PayloadSchema::List => {
            trimmed(map, path, "projectId", false, None)?;
            max_items(map, path, "projectIds", 0, 100)?;
            each_item(map, path, "projectIds", |item, at| trimmed_value(item, at, None))?;
            trimmed(map, path, "host", false, None)?;
            if let Some(limit) = map.get("limit") {
                let at = field_path(path, "limit");
                match limit.as_f64() {
                    Some(number) if number.fract() == 0.0 && (1.0..=500.0).contains(&number) => {}
                    _ => return Err(issue(&at, format!("Expected a value between 1 and 500, got {limit}"))),
                }
            }
            if let Some(cursors) = map.get_mut("cursors") {
                let at = field_path(path, "cursors");
                let Value::Object(entries) = cursors else {
                    return Err(issue(&at, format!("Expected object, got {cursors}")));
                };
                let mut next = Map::new();
                for (key, mut cursor) in std::mem::take(entries) {
                    let trimmed_key = trim(&key).to_owned();
                    if trimmed_key.is_empty() {
                        return Err(issue(&at, "Expected a value with a length of at least 1, got \"\""));
                    }
                    trimmed_value(&mut cursor, &field_path(&at, &key), Some(4096))?;
                    next.insert(trimmed_key, cursor);
                }
                *entries = next;
            }
            trimmed(map, path, "query", false, Some(200))?;
            if let Some(filters) = map.get_mut("filters") {
                let at = field_path(path, "filters");
                let filters = object(filters, &at)?;
                max_items(filters, &at, "labels", 0, 10)?;
                each_item(filters, &at, "labels", |group, group_at| {
                    let Value::Array(values) = group else {
                        return Err(issue(group_at, format!("Expected array, got {group}")));
                    };
                    if values.len() > 10 {
                        return Err(issue(group_at, "Expected a value with a length of at most 10"));
                    }
                    for (index, value) in values.iter_mut().enumerate() {
                        trimmed_value(value, &format!("{group_at}[{index}]"), Some(200))?;
                    }
                    Ok(())
                })?;
                max_items(filters, &at, "excludedLabels", 0, 10)?;
                each_item(filters, &at, "excludedLabels", |value, value_at| trimmed_value(value, value_at, Some(200)))?;
                trimmed(filters, &at, "author", false, Some(200))?;
            }
        }
        PayloadSchema::ListStats => {
            max_items(map, path, "refs", 0, 500)?;
            each_item(map, path, "refs", reference_value)?;
        }
        PayloadSchema::RoutingIdentity => trimmed(map, path, "host", true, None)?,
        PayloadSchema::ThreadComments => {
            reference_fields(map, path)?;
            trimmed(map, path, "threadId", true, None)?;
            trimmed(map, path, "cursor", true, None)?;
        }
        PayloadSchema::Diff => {
            reference_fields(map, path)?;
            trimmed(map, path, "cursor", false, None)?;
            trimmed(map, path, "commit", false, None)?;
        }
        PayloadSchema::DiffFileContents => {
            reference_fields(map, path)?;
            trimmed(map, path, "commit", false, None)?;
            trimmed(map, path, "oldPath", true, None)?;
            trimmed(map, path, "newPath", true, None)?;
        }
        PayloadSchema::SetFilesViewed => {
            reference_fields(map, path)?;
            max_items(map, path, "files", 0, 500)?;
            each_item(map, path, "files", |file, at| {
                let file = object(file, at)?;
                bounded_string(file, at, "path", true, FILE_PATH_MAX)
            })?;
        }
        PayloadSchema::Action => {
            positive_int(map, path, "stackNumber")?;
            each_item(map, path, "expectedStackHeads", |head, at| {
                let head = object(head, at)?;
                positive_int(head, at, "number")?;
                trimmed(head, at, "headSha", true, None)
            })?;
            reference_fields(map, path)?;
        }
        PayloadSchema::Update => {
            reference_fields(map, path)?;
            trimmed(map, path, "title", false, Some(1024))?;
            bounded_string(map, path, "body", false, COMMENT_BODY_MAX)?;
        }
        PayloadSchema::Comment => {
            reference_fields(map, path)?;
            bounded_string(map, path, "body", true, COMMENT_BODY_MAX)?;
        }
        PayloadSchema::CommentUpdate => {
            reference_fields(map, path)?;
            trimmed(map, path, "commentId", true, None)?;
            bounded_string(map, path, "body", true, COMMENT_BODY_MAX)?;
        }
        PayloadSchema::SubmitReview => {
            reference_fields(map, path)?;
            bounded_string(map, path, "body", false, COMMENT_BODY_MAX)?;
            each_item(map, path, "comments", |draft, at| {
                let draft = object(draft, at)?;
                trimmed(draft, at, "path", true, None)?;
                trimmed(draft, at, "oldPath", false, None)?;
                bounded_string(draft, at, "body", true, COMMENT_BODY_MAX)?;
                if let Some(position) = draft.get_mut("position") {
                    let position_at = field_path(at, "position");
                    let position = object(position, &position_at)?;
                    positive_int(position, &position_at, "newLine")?;
                    positive_int(position, &position_at, "oldLine")?;
                }
                Ok(())
            })?;
        }
        PayloadSchema::ThreadReply => {
            reference_fields(map, path)?;
            trimmed(map, path, "threadId", true, None)?;
            bounded_string(map, path, "body", true, COMMENT_BODY_MAX)?;
        }
        PayloadSchema::ThreadResolution => {
            reference_fields(map, path)?;
            trimmed(map, path, "threadId", true, None)?;
        }
        PayloadSchema::Reaction => {
            reference_fields(map, path)?;
            trimmed(map, path, "subjectId", false, None)?;
        }
        PayloadSchema::ReviewerRequest => {
            reference_fields(map, path)?;
            max_items(map, path, "reviewers", 1, 25)?;
            each_item(map, path, "reviewers", |reviewer, at| {
                let reviewer = object(reviewer, at)?;
                trimmed(reviewer, at, "id", true, None)
            })?;
        }
        PayloadSchema::LabelChange => {
            reference_fields(map, path)?;
            max_items(map, path, "labels", 1, 25)?;
            each_item(map, path, "labels", |label, at| trimmed_value(label, at, None))?;
        }
        PayloadSchema::Invalidate => {
            if let Some(reference) = map.get_mut("reference") {
                reference_value(reference, &field_path(path, "reference"))?;
            }
        }
    }
    Ok(())
}

/// Normalizes then deserializes a payload.
pub fn decode<T: DeserializeOwned>(schema: PayloadSchema, mut value: Value) -> Result<T, DecodeIssue> {
    normalize(schema, &mut value)?;
    serde_json::from_value(value).map_err(|error| DecodeIssue(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use zc_contracts::{PullRequestListInput, PullRequestRef};

    #[test]
    fn trims_references_and_refuses_empty_or_non_positive_fields() {
        let reference: PullRequestRef = decode(
            PayloadSchema::Ref,
            json!({"projectId": " project-1 ", "repository": " acme/widgets ", "number": 7, "host": " github.com "}),
        )
        .unwrap();
        assert_eq!((reference.project_id.as_str(), reference.repository.as_str()), ("project-1", "acme/widgets"));
        assert_eq!(reference.host.as_deref(), Some("github.com"));
        assert!(decode::<PullRequestRef>(PayloadSchema::Ref, json!({"projectId": "p", "repository": "  ", "number": 7})).is_err());
        assert!(decode::<PullRequestRef>(PayloadSchema::Ref, json!({"projectId": "p", "repository": "a/b", "number": 0})).is_err());
        assert!(decode::<PullRequestRef>(PayloadSchema::Ref, json!({"projectId": "p", "repository": "a/b", "number": 1.5})).is_err());
    }

    #[test]
    fn list_inputs_trim_cursor_keys_and_bound_the_limit() {
        let input: PullRequestListInput = decode(
            PayloadSchema::List,
            json!({"state": "open", "limit": 50, "cursors": {" github.com acme/widgets ": " 2026-01-01T00:00:00Z|3| "}, "query": "  fix  "}),
        )
        .unwrap();
        let cursors = input.cursors.unwrap();
        assert_eq!(cursors.get("github.com acme/widgets").map(String::as_str), Some("2026-01-01T00:00:00Z|3|"));
        assert_eq!(input.query.as_deref(), Some("fix"));
        assert!(decode::<PullRequestListInput>(PayloadSchema::List, json!({"state": "open", "limit": 501})).is_err());
        assert!(decode::<PullRequestListInput>(PayloadSchema::List, json!({"state": "open", "query": " "})).is_err());
    }

    #[test]
    fn comment_bodies_are_kept_verbatim_but_must_not_be_empty() {
        let mut value = json!({"projectId": "p", "repository": "a/b", "number": 1, "body": "  hi  "});
        normalize(PayloadSchema::Comment, &mut value).unwrap();
        assert_eq!(value["body"], json!("  hi  "));
        let mut empty = json!({"projectId": "p", "repository": "a/b", "number": 1, "body": ""});
        assert!(normalize(PayloadSchema::Comment, &mut empty).is_err());
    }
}
