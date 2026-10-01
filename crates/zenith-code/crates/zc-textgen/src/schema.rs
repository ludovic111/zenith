//! The structured outputs the prompts ask for (`outputSchema` of `TextGenerationPrompts.ts`):
//! their JSON Schema as `toJsonSchemaObject` renders it (type side, closed objects), and their
//! decoding (Effect `Schema.decode`: required string fields, extra keys dropped,
//! `needsRefinement` defaulting to false).

use serde_json::{json, Map, Value};

/// One of the five output shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSchema {
    /// `{subject, body}`.
    CommitMessage,
    /// `{subject, body, branch}`.
    CommitMessageWithBranch,
    /// `{title, body}`.
    PrContent,
    /// `{branch}`.
    BranchName,
    /// `{title, needsRefinement}` (`needsRefinement` defaults to false when absent).
    ThreadTitle,
}

/// A decoded structured output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    CommitMessage { subject: String, body: String, branch: Option<String> },
    PrContent { title: String, body: String },
    BranchName { branch: String },
    ThreadTitle { title: String, needs_refinement: bool },
}

impl OutputSchema {
    fn string_fields(self) -> &'static [&'static str] {
        match self {
            Self::CommitMessage => &["subject", "body"],
            Self::CommitMessageWithBranch => &["subject", "body", "branch"],
            Self::PrContent => &["title", "body"],
            Self::BranchName => &["branch"],
            Self::ThreadTitle => &["title"],
        }
    }

    /// `toJsonSchemaObject(outputSchema)`.
    pub fn json_schema(self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for field in self.string_fields() {
            properties.insert((*field).to_owned(), json!({"type": "string"}));
            required.push(json!(field));
        }
        if self == Self::ThreadTitle {
            properties.insert("needsRefinement".into(), json!({"type": "boolean"}));
            required.push(json!("needsRefinement"));
        }
        json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
    }

    /// The JSON Schema as the compact JSON string the CLIs receive.
    pub fn json_schema_string(self) -> String {
        self.json_schema().to_string()
    }

    /// `Schema.decodeEffect(outputSchema)(value)`; `None` is a schema error.
    pub fn decode(self, value: &Value) -> Option<Decoded> {
        let object = value.as_object()?;
        let string = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(match self {
            Self::CommitMessage => Decoded::CommitMessage {
                subject: string("subject")?,
                body: string("body")?,
                branch: None,
            },
            Self::CommitMessageWithBranch => Decoded::CommitMessage {
                subject: string("subject")?,
                body: string("body")?,
                branch: Some(string("branch")?),
            },
            Self::PrContent => Decoded::PrContent {
                title: string("title")?,
                body: string("body")?,
            },
            Self::BranchName => Decoded::BranchName { branch: string("branch")? },
            Self::ThreadTitle => Decoded::ThreadTitle {
                title: string("title")?,
                needs_refinement: match object.get("needsRefinement") {
                    None => false,
                    Some(Value::Bool(flag)) => *flag,
                    Some(_) => return None,
                },
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_closed_schemas_like_effect() {
        assert_eq!(
            OutputSchema::ThreadTitle.json_schema_string(),
            r#"{"type":"object","properties":{"title":{"type":"string"},"needsRefinement":{"type":"boolean"}},"required":["title","needsRefinement"],"additionalProperties":false}"#
        );
        assert_eq!(
            OutputSchema::CommitMessageWithBranch.json_schema_string(),
            r#"{"type":"object","properties":{"subject":{"type":"string"},"body":{"type":"string"},"branch":{"type":"string"}},"required":["subject","body","branch"],"additionalProperties":false}"#
        );
    }

    #[test]
    fn decodes_like_effect_schema() {
        let title = OutputSchema::ThreadTitle;
        assert_eq!(
            title.decode(&json!({"title": "a", "extra": 1})),
            Some(Decoded::ThreadTitle {
                title: "a".into(),
                needs_refinement: false
            })
        );
        assert_eq!(title.decode(&json!({"title": "a", "needsRefinement": null})), None);
        assert_eq!(title.decode(&json!({"title": 42})), None);
        assert_eq!(title.decode(&Value::Null), None);
        assert_eq!(OutputSchema::BranchName.decode(&json!({"title": "not a branch"})), None);
    }
}
