//! MCP elicitation approvals (`describeMcpElicitation`, `toMcpElicitationResponse` of
//! `CodexSessionRuntime.ts`): which approval choices an app-access prompt offers, and the wire
//! answer for a decision.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};
use zc_contracts::ProviderApprovalDecision;

/// `McpElicitationMetadata`, read only when every listed key has the expected type.
#[derive(Debug, Default)]
struct Metadata {
    app: Option<String>,
    app_name: Option<String>,
    app_name_camel: Option<String>,
    connector_name: Option<String>,
    connector_name_camel: Option<String>,
    allow_persistent_approval: Option<bool>,
    persist: Vec<String>,
    target_app: Option<String>,
    target_name: Option<String>,
    tool_params_app: Option<String>,
    tool_params_app_name: Option<String>,
}

/// `optionalKey(NullOr(String))`: absent or null → None, string → Some, anything else → invalid.
fn nullable_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn nested(object: &Map<String, Value>, key: &str) -> Result<Option<Map<String, Value>>, ()> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(value)) => Ok(Some(value.clone())),
        Some(_) => Err(()),
    }
}

fn read_metadata(value: Option<&Value>) -> Option<Metadata> {
    let object = value?.as_object()?;
    let parse = || -> Result<Metadata, ()> {
        let target = nested(object, "target")?;
        let tool_params = nested(object, "tool_params")?;
        let persist = match object.get("persist") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::String(value)) => vec![value.clone()],
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| value.as_str().map(str::to_owned).ok_or(()))
                .collect::<Result<_, _>>()?,
            Some(_) => return Err(()),
        };
        let allow_persistent_approval = match object.get("allowPersistentApproval") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(*value),
            Some(_) => return Err(()),
        };
        Ok(Metadata {
            app: nullable_string(object, "app")?,
            app_name: nullable_string(object, "app_name")?,
            app_name_camel: nullable_string(object, "appName")?,
            connector_name: nullable_string(object, "connector_name")?,
            connector_name_camel: nullable_string(object, "connectorName")?,
            allow_persistent_approval,
            persist,
            target_app: target.as_ref().map(|target| nullable_string(target, "app")).transpose()?.flatten(),
            target_name: target.as_ref().map(|target| nullable_string(target, "name")).transpose()?.flatten(),
            tool_params_app: tool_params.as_ref().map(|params| nullable_string(params, "app")).transpose()?.flatten(),
            tool_params_app_name: tool_params.as_ref().map(|params| nullable_string(params, "app_name")).transpose()?.flatten(),
        })
    };
    parse().ok()
}

/// `McpElicitationFormField`.
#[derive(Debug, Clone)]
struct FormField {
    r#type: Option<String>,
    title: Option<String>,
    description: Option<String>,
    default: Option<Value>,
    r#enum: Option<Vec<String>>,
    enum_names: Option<Vec<String>>,
    one_of: Option<Vec<(String, Option<String>)>>,
}

fn nullable_strings(object: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, ()> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| value.as_str().map(str::to_owned).ok_or(()))
            .collect::<Result<_, _>>()
            .map(Some),
        Some(_) => Err(()),
    }
}

fn read_field(value: &Value) -> Result<FormField, ()> {
    let object = value.as_object().ok_or(())?;
    let one_of = match object.get("oneOf") {
        None | Some(Value::Null) => None,
        Some(Value::Array(options)) => Some(
            options
                .iter()
                .map(|option| {
                    let option = option.as_object().ok_or(())?;
                    let constant = option.get("const").and_then(Value::as_str).ok_or(())?.to_owned();
                    Ok((constant, nullable_string(option, "title")?))
                })
                .collect::<Result<Vec<_>, ()>>()?,
        ),
        Some(_) => return Err(()),
    };
    Ok(FormField {
        r#type: nullable_string(object, "type")?,
        title: nullable_string(object, "title")?,
        description: nullable_string(object, "description")?,
        default: object.get("default").cloned(),
        r#enum: nullable_strings(object, "enum")?,
        enum_names: nullable_strings(object, "enumNames")?,
        one_of,
    })
}

/// `McpElicitationForm`: `{properties?: Record<string, field>, required?: string[] | null}`.
struct Form {
    properties: Vec<(String, FormField)>,
    required: Option<Vec<String>>,
}

fn form_fields(payload: &Value) -> Option<Form> {
    if payload.get("mode").and_then(Value::as_str) == Some("url") {
        return None;
    }
    let object = payload.get("requestedSchema")?.as_object()?;
    let properties = match object.get("properties") {
        None => Vec::new(),
        Some(Value::Object(properties)) => properties
            .iter()
            .map(|(key, field)| read_field(field).map(|field| (key.clone(), field)))
            .collect::<Result<_, _>>()
            .ok()?,
        Some(_) => return None,
    };
    let required = nullable_strings(object, "required").ok()?;
    Some(Form { properties, required })
}

fn field_options(field: &FormField) -> Vec<(String, Option<String>)> {
    if let Some(one_of) = &field.one_of {
        return one_of.clone();
    }
    field
        .r#enum
        .iter()
        .flatten()
        .enumerate()
        .map(|(index, value)| (value.clone(), field.enum_names.as_ref().and_then(|names| names.get(index).cloned())))
        .collect()
}

fn persistence_decision(value: &str) -> Option<ProviderApprovalDecision> {
    let normalized = value.to_lowercase();
    if normalized.contains("session") {
        return Some(ProviderApprovalDecision::AcceptForSession);
    }
    if ["always", "permanent", "forever", "persistent"].iter().any(|word| normalized.contains(word)) {
        return Some(ProviderApprovalDecision::AcceptAlways);
    }
    None
}

fn is_persistence_field(key: &str, field: &FormField) -> bool {
    persistence_decision(key).is_some()
        || key.to_lowercase() == "persist"
        || persistence_decision(field.title.as_deref().unwrap_or_default()).is_some()
        || persistence_decision(field.description.as_deref().unwrap_or_default()).is_some()
}

fn allow_chatgpt_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)^Allow ChatGPT to use (.+?)\?$").expect("valid regex"))
}

fn once_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| Regex::new(r"(?i)once|accept|approve|allow").expect("valid regex"))
}

/// One approval choice (`ProviderApprovalOption`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalOption {
    pub decision: ProviderApprovalDecision,
    pub label: String,
}

/// What `describeMcpElicitation` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElicitationApproval {
    pub app_name: String,
    pub options: Vec<ApprovalOption>,
}

impl ElicitationApproval {
    pub fn options_value(&self) -> Value {
        Value::Array(
            self.options
                .iter()
                .map(|option| json!({"decision": option.decision.as_str(), "label": option.label}))
                .collect(),
        )
    }
}

/// `describeMcpElicitation`: the app and the approval choices an MCP elicitation advertises.
pub fn describe_mcp_elicitation(payload: &Value) -> ElicitationApproval {
    let metadata = read_metadata(payload.get("_meta"));
    let message = payload.get("message").and_then(Value::as_str).unwrap_or_default();
    let from_message = allow_chatgpt_regex()
        .captures(message)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_owned());
    let metadata_ref = metadata.as_ref();
    let app_name = metadata_ref
        .and_then(|m| m.app_name.clone())
        .or_else(|| metadata_ref.and_then(|m| m.app_name_camel.clone()))
        .or_else(|| metadata_ref.and_then(|m| m.app.clone()))
        .or_else(|| metadata_ref.and_then(|m| m.target_app.clone()))
        .or_else(|| metadata_ref.and_then(|m| m.target_name.clone()))
        .or_else(|| metadata_ref.and_then(|m| m.tool_params_app_name.clone()))
        .or_else(|| metadata_ref.and_then(|m| m.tool_params_app.clone()))
        .or(from_message)
        .or_else(|| metadata_ref.and_then(|m| m.connector_name.clone()))
        .or_else(|| metadata_ref.and_then(|m| m.connector_name_camel.clone()))
        .unwrap_or_else(|| payload.get("serverName").and_then(Value::as_str).unwrap_or_default().to_owned());

    // Insertion-ordered (decision, label), like the TS Map.
    let mut persistence: Vec<(ProviderApprovalDecision, String)> = Vec::new();
    let mut set = |decision: ProviderApprovalDecision, label: String| {
        if let Some(entry) = persistence.iter_mut().find(|(existing, _)| *existing == decision) {
            entry.1 = label;
        } else {
            persistence.push((decision, label));
        }
    };
    if let Some(metadata) = metadata_ref {
        for value in &metadata.persist {
            if let Some(decision) = persistence_decision(value) {
                set(decision, String::new());
            }
        }
        if metadata.allow_persistent_approval == Some(true) {
            set(ProviderApprovalDecision::AcceptAlways, String::new());
        }
    }
    if let Some(form) = form_fields(payload) {
        for (key, field) in &form.properties {
            for (value, label) in field_options(field) {
                if let Some(decision) = persistence_decision(&value) {
                    set(decision, label.unwrap_or_default());
                }
            }
            if field.r#type.as_deref() == Some("boolean") && is_persistence_field(key, field) {
                set(ProviderApprovalDecision::AcceptAlways, field.title.clone().unwrap_or_default());
            }
        }
    }
    let label_of = |decision: ProviderApprovalDecision| persistence.iter().find(|(existing, _)| *existing == decision).map(|(_, label)| label.clone());

    let mut options = vec![
        ApprovalOption {
            decision: ProviderApprovalDecision::Cancel,
            label: "Cancel".into(),
        },
        ApprovalOption {
            decision: ProviderApprovalDecision::Decline,
            label: "Decline".into(),
        },
    ];
    for (decision, fallback) in [
        (ProviderApprovalDecision::AcceptForSession, "Always allow this session"),
        (ProviderApprovalDecision::AcceptAlways, "Always allow"),
    ] {
        if let Some(label) = label_of(decision) {
            if to_mcp_elicitation_response(payload, decision)["action"] == "accept" {
                options.push(ApprovalOption {
                    decision,
                    label: if label.is_empty() { fallback.to_owned() } else { label },
                });
            }
        }
    }
    options.push(ApprovalOption {
        decision: ProviderApprovalDecision::Accept,
        label: "Approve".into(),
    });
    ElicitationApproval { app_name, options }
}

/// `toMcpElicitationResponse`: the MCP elicitation answer for a decision.
pub fn to_mcp_elicitation_response(payload: &Value, decision: ProviderApprovalDecision) -> Value {
    if matches!(decision, ProviderApprovalDecision::Decline | ProviderApprovalDecision::Cancel) {
        return json!({ "action": decision.as_str() });
    }
    if payload.get("mode").and_then(Value::as_str) == Some("url") {
        return json!({ "action": "decline" });
    }
    let persist = match decision {
        ProviderApprovalDecision::AcceptForSession => Some("session"),
        ProviderApprovalDecision::AcceptAlways => Some("always"),
        _ => None,
    };
    let form = form_fields(payload);
    let mut content = Map::new();
    for (key, field) in form.iter().flat_map(|form| form.properties.iter()) {
        let chosen = field_options(field).into_iter().find(|(value, _)| {
            if persist.is_some() {
                persistence_decision(value) == Some(decision)
            } else {
                once_regex().is_match(value) && persistence_decision(value).is_none()
            }
        });
        if let Some((value, _)) = chosen {
            content.insert(key.clone(), Value::String(value));
        } else if field.r#type.as_deref() == Some("boolean") && is_persistence_field(key, field) {
            content.insert(key.clone(), Value::Bool(decision == ProviderApprovalDecision::AcceptAlways));
        } else if let Some(default) = field.default.as_ref().filter(|default| !default.is_null()) {
            content.insert(key.clone(), default.clone());
        }
    }
    if let Some(required) = form.as_ref().and_then(|form| form.required.as_ref()) {
        if required.iter().any(|key| !content.contains_key(key)) {
            return json!({ "action": "decline" });
        }
    }
    let mut response = Map::new();
    response.insert("action".into(), json!("accept"));
    if let Some(persist) = persist {
        response.insert("_meta".into(), json!({ "persist": persist }));
    }
    if form.is_some() {
        response.insert("content".into(), Value::Object(content));
    }
    Value::Object(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProviderApprovalDecision::*;

    fn request() -> Value {
        json!({
            "mode": "form",
            "message": "Allow ChatGPT to use Safari?",
            "serverName": "computer-use",
            "threadId": "provider-thread-1",
            "turnId": "turn-1",
            "_meta": {"app_name": "Safari", "persist": ["session", "always"]},
            "requestedSchema": {"type": "object", "properties": {"approval": {"type": "string", "oneOf": [
                {"const": "once", "title": "Allow once"},
                {"const": "session", "title": "Allow for this session"},
                {"const": "always", "title": "Always allow Safari"}
            ]}}, "required": ["approval"]}
        })
    }

    fn with(base: Value, patch: Value) -> Value {
        let mut base = base;
        for (key, value) in patch.as_object().unwrap() {
            base[key] = value.clone();
        }
        base
    }

    fn option(decision: ProviderApprovalDecision, label: &str) -> ApprovalOption {
        ApprovalOption { decision, label: label.into() }
    }

    #[test]
    fn preserves_app_name_and_persistence_choices() {
        assert_eq!(
            describe_mcp_elicitation(&request()),
            ElicitationApproval {
                app_name: "Safari".into(),
                options: vec![
                    option(Cancel, "Cancel"),
                    option(Decline, "Decline"),
                    option(AcceptForSession, "Allow for this session"),
                    option(AcceptAlways, "Always allow Safari"),
                    option(Accept, "Approve"),
                ],
            }
        );
    }

    #[test]
    fn app_name_from_the_message_without_metadata() {
        let mut payload = request();
        payload.as_object_mut().unwrap().remove("_meta");
        assert_eq!(describe_mcp_elicitation(&payload).app_name, "Safari");
    }

    #[test]
    fn responses_for_each_decision() {
        assert_eq!(
            to_mcp_elicitation_response(&request(), Accept),
            json!({"action": "accept", "content": {"approval": "once"}})
        );
        assert_eq!(
            to_mcp_elicitation_response(&request(), AcceptForSession),
            json!({"action": "accept", "_meta": {"persist": "session"}, "content": {"approval": "session"}})
        );
        assert_eq!(
            to_mcp_elicitation_response(&request(), AcceptAlways),
            json!({"action": "accept", "_meta": {"persist": "always"}, "content": {"approval": "always"}})
        );
        assert_eq!(to_mcp_elicitation_response(&request(), Decline), json!({"action": "decline"}));
        assert_eq!(to_mcp_elicitation_response(&request(), Cancel), json!({"action": "cancel"}));
    }

    #[test]
    fn boolean_permanent_approval_fields() {
        let payload = with(
            request(),
            json!({"_meta": {"app_name": "Safari"}, "requestedSchema": {"type": "object", "properties": {"always": {"type": "boolean", "title": "Always allow Safari"}}}}),
        );
        assert!(describe_mcp_elicitation(&payload).options.iter().any(|option| option.decision == AcceptAlways));
        assert_eq!(
            to_mcp_elicitation_response(&payload, AcceptAlways),
            json!({"action": "accept", "_meta": {"persist": "always"}, "content": {"always": true}})
        );
    }

    #[test]
    fn nullable_fields_and_persistence_choices() {
        let payload = with(
            request(),
            json!({
                "_meta": {"app_name": null, "appName": "Safari", "connector_name": null, "persist": null, "target": null, "tool_params": null},
                "requestedSchema": {"type": "object", "properties": {"approval": {"type": "string", "title": null, "description": null, "default": null, "enum": ["once", "always"], "enumNames": null}}, "required": ["approval"]}
            }),
        );
        assert_eq!(describe_mcp_elicitation(&payload).app_name, "Safari");
        assert!(describe_mcp_elicitation(&payload).options.iter().any(|option| option.decision == AcceptAlways));
        assert_eq!(
            to_mcp_elicitation_response(&payload, AcceptAlways),
            json!({"action": "accept", "_meta": {"persist": "always"}, "content": {"approval": "always"}})
        );
    }

    #[test]
    fn declines_required_fields_an_approval_cannot_collect() {
        let payload = with(
            request(),
            json!({"requestedSchema": {"type": "object", "properties": {"email": {"type": "string", "format": "email"}}, "required": ["email"]}}),
        );
        assert_eq!(to_mcp_elicitation_response(&payload, Accept), json!({"action": "decline"}));
    }

    #[test]
    fn url_elicitations_are_declined() {
        let payload = json!({"mode": "url", "message": "Finish signing in to continue.", "serverName": "computer-use", "threadId": "t", "turnId": "u", "elicitationId": "sign-in-1", "url": "https://example.com/authorize"});
        assert_eq!(to_mcp_elicitation_response(&payload, Accept), json!({"action": "decline"}));
    }

    #[test]
    fn omits_persistence_choices_that_cannot_satisfy_required_fields() {
        let payload = with(
            request(),
            json!({"_meta": {"app_name": "Safari", "persist": ["session", "always"]}, "requestedSchema": {"type": "object", "properties": {"approval": {"type": "string", "enum": ["once"]}}, "required": ["approval"]}}),
        );
        assert_eq!(
            describe_mcp_elicitation(&payload).options,
            vec![option(Cancel, "Cancel"), option(Decline, "Decline"), option(Accept, "Approve")]
        );
    }
}
