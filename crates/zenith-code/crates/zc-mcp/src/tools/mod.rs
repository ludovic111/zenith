//! The `t3-code` toolkits behind one [`Toolkit`]: what `tools/list` returns and what
//! `tools/call` runs.
//!
//! Results take the shapes of Effect's `McpServer.toolkit` and of the two hand-registered image
//! tools (`McpHttpServer.ts`):
//!
//! - success: `{content:[{type:"text", text:JSON}], structuredContent:<object>, isError:false}`,
//!   the value encoded with the tool's success schema first (unknown keys dropped, schema
//!   order kept);
//! - a declared failure: `{content:[{type:"text", text:<error message>}], isError:true}`;
//! - an internal failure: the same with "Tool execution failed due to an internal server
//!   error.";
//! - parameters that do not decode: a JSON-RPC `InvalidParams` error ([`ToolCallError`]).

pub mod device;
pub mod preview;
pub mod pull_requests;
pub mod snapshot;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Map, Value};
use zc_ports::TaggedError;

pub use pull_requests::{DispatchFailure, OrchestrationPullRequests, PullRequestBackend};

use crate::broker::PreviewAutomationBroker;
use crate::params::{decode_arguments, Field};
use crate::scope::McpInvocationScope;

/// The services the tools use.
pub struct McpServices {
    pub broker: PreviewAutomationBroker,
    pub pull_requests: Arc<dyn PullRequestBackend>,
    /// `ServerConfig.attachmentsDir` (recordings are claimed into it).
    pub attachments_dir: PathBuf,
    /// `ServerConfig.browserArtifactsDir` (saved snapshot screenshots).
    pub browser_artifacts_dir: PathBuf,
}

/// A `tools/call` that fails at the protocol level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallError {
    /// `Tool '<name>' not found`.
    NotFound(String),
    /// `Invalid parameters for tool '<name>': <issue>`.
    InvalidParams(String),
}

impl ToolCallError {
    pub fn message(&self) -> String {
        match self {
            Self::NotFound(name) => format!("Tool '{name}' not found"),
            Self::InvalidParams(message) => message.clone(),
        }
    }
}

/// `INTERNAL_TOOL_ERROR_MESSAGE`.
pub const INTERNAL_TOOL_ERROR_MESSAGE: &str = "Tool execution failed due to an internal server error.";

struct Catalog {
    tools: Vec<Value>,
    output_schemas: HashMap<String, Value>,
    success_schemas: HashMap<String, Value>,
}

fn brand_strings(value: &mut Value) {
    match value {
        Value::String(text) if text.contains("T3 Code") => *text = crate::brand(text),
        Value::Array(items) => items.iter_mut().for_each(brand_strings),
        Value::Object(map) => map.values_mut().for_each(brand_strings),
        _ => {}
    }
}

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut document: Value = serde_json::from_str(include_str!("../tools.json")).expect("tools.json is valid JSON");
        brand_strings(&mut document);
        let tools: Vec<Value> = document["tools"].as_array().cloned().unwrap_or_default();
        let output_schemas = tools
            .iter()
            .filter_map(|tool| Some((tool["name"].as_str()?.to_owned(), tool.get("outputSchema")?.clone())))
            .collect();
        let success_schemas = document["successSchemas"]
            .as_object()
            .map(|schemas| schemas.iter().map(|(name, schema)| (name.clone(), schema.clone())).collect())
            .unwrap_or_default();
        Catalog {
            tools,
            output_schemas,
            success_schemas,
        }
    })
}

/// The tool descriptors `tools/list` returns, in registration order.
pub fn descriptors() -> &'static [Value] {
    &catalog().tools
}

/// The success schema a tool's result is encoded with.
pub fn success_schema(tool: &str) -> Option<&'static Value> {
    let catalog = catalog();
    catalog.output_schemas.get(tool).or_else(|| catalog.success_schemas.get(tool))
}

/// Encodes `value` with a JSON Schema the way Effect's encoder treats the success schema it was
/// generated from: object properties in schema order, other keys dropped, unions resolved to
/// the first member that takes the value. `None` when the value does not fit (Effect fails to
/// encode it).
pub fn encode_with_schema(value: &Value, schema: &Value) -> Option<Value> {
    if let Some(members) = schema.get("anyOf").and_then(Value::as_array) {
        return members.iter().find_map(|member| encode_with_schema(value, member));
    }
    let Some(kind) = schema.get("type").and_then(Value::as_str) else {
        return Some(value.clone());
    };
    let fits_enum = |value: &Value| schema.get("enum").and_then(Value::as_array).is_none_or(|options| options.contains(value));
    match kind {
        "object" => {
            let object = value.as_object()?;
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                if required.iter().filter_map(Value::as_str).any(|key| !object.contains_key(key)) {
                    return None;
                }
            }
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return Some(value.clone());
            };
            let mut out = Map::new();
            for (key, property) in properties {
                if let Some(field) = object.get(key) {
                    out.insert(key.clone(), encode_with_schema(field, property)?);
                }
            }
            Some(Value::Object(out))
        }
        "array" => {
            let items = value.as_array()?;
            match schema.get("items") {
                Some(item_schema) => items
                    .iter()
                    .map(|item| encode_with_schema(item, item_schema))
                    .collect::<Option<Vec<_>>>()
                    .map(Value::Array),
                None => Some(value.clone()),
            }
        }
        "string" => (value.is_string() && fits_enum(value)).then(|| value.clone()),
        "number" => value.is_number().then(|| value.clone()),
        "integer" => value.as_f64().filter(|n| value.is_number() && n.fract() == 0.0).map(|_| value.clone()),
        "boolean" => value.is_boolean().then(|| value.clone()),
        "null" => value.is_null().then(|| value.clone()),
        _ => Some(value.clone()),
    }
}

/// A text content block.
pub fn text_content(text: impl Into<String>) -> Value {
    json!({"type": "text", "text": text.into()})
}

/// A successful toolkit result: the encoded value as JSON text and, when it is an object, as
/// `structuredContent`.
pub fn success_result(encoded: &Value) -> Value {
    let mut result = Map::new();
    result.insert("content".into(), json!([text_content(serde_json::to_string(encoded).unwrap_or_default())]));
    if encoded.is_object() {
        result.insert("structuredContent".into(), encoded.clone());
    }
    result.insert("isError".into(), json!(false));
    Value::Object(result)
}

/// `toolErrorResult(message)`.
pub fn error_result(message: &str) -> Value {
    json!({"content": [text_content(message)], "isError": true})
}

/// The toolkits.
#[derive(Clone)]
pub struct Toolkit {
    services: Arc<McpServices>,
}

impl Toolkit {
    pub fn new(services: McpServices) -> Self {
        Self { services: Arc::new(services) }
    }

    pub fn services(&self) -> &McpServices {
        &self.services
    }

    /// `tools/list`.
    pub fn list(&self) -> Vec<Value> {
        descriptors().to_vec()
    }

    fn decode(tool: &str, fields: &[Field], arguments: Option<&Value>, closed: bool) -> Result<Map<String, Value>, ToolCallError> {
        decode_arguments(fields, arguments, closed)
            .map_err(|issue| ToolCallError::InvalidParams(format!("Invalid parameters for tool '{tool}': {}", issue.render())))
    }

    /// Encodes a toolkit handler's outcome into its `CallToolResult`.
    fn finish(tool: &str, outcome: Result<Value, TaggedError>) -> Value {
        match outcome {
            Ok(value) => {
                let encoded = match success_schema(tool) {
                    Some(schema) => encode_with_schema(&value, schema),
                    None => Some(value),
                };
                match encoded {
                    Some(encoded) => success_result(&encoded),
                    None => {
                        tracing::error!(tool, "tool handler returned a result its success schema does not take");
                        error_result(INTERNAL_TOOL_ERROR_MESSAGE)
                    }
                }
            }
            Err(error) => {
                tracing::debug!(tool, tag = %error.tag, "tool failed");
                error_result(&error.message)
            }
        }
    }

    /// `tools/call`.
    pub async fn call(&self, name: &str, arguments: Option<&Value>, scope: &McpInvocationScope) -> Result<Value, ToolCallError> {
        let services = &self.services;
        let pull_requests = pull_requests::PullRequestTools {
            backend: services.pull_requests.clone(),
        };
        let preview = preview::PreviewTools { services };
        let outcome = match name {
            "link_pull_request" => {
                let input = Self::decode(name, &pull_requests::target_fields(), arguments, false)?;
                pull_requests.link(scope, &input).await
            }
            "unlink_pull_request" => {
                let input = Self::decode(name, &pull_requests::target_fields(), arguments, false)?;
                pull_requests.unlink(scope, &input).await
            }
            "list_thread_pull_requests" => {
                Self::decode(name, &[], arguments, true)?;
                pull_requests.list(scope).await
            }
            "preview_snapshot" => return Ok(snapshot::call(services, arguments, scope).await),
            "device_screenshot" => return Ok(device::screenshot(arguments, scope)),
            "device_list" | "device_open" | "device_close" => {
                let input = Self::decode(name, &device::fields(name), arguments, false)?;
                device::call(name, &input, scope)
            }
            _ if preview::is_preview_tool(name) => {
                let input = Self::decode(name, &preview::fields(name), arguments, false)?;
                if let Some(message) = preview::check(name, &input) {
                    return Err(ToolCallError::InvalidParams(format!("Invalid parameters for tool '{name}': {message}")));
                }
                preview.call(name, input, scope).await
            }
            _ => return Err(ToolCallError::NotFound(name.to_owned())),
        };
        Ok(Self::finish(name, outcome))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_the_typescript_tool_list_with_the_brand() {
        let names: Vec<&str> = descriptors().iter().filter_map(|tool| tool["name"].as_str()).collect();
        assert_eq!(names.len(), 21);
        assert!(names.contains(&"link_pull_request") && names.contains(&"preview_snapshot") && names.contains(&"device_screenshot"));
        let link = descriptors().iter().find(|tool| tool["name"] == "link_pull_request").unwrap();
        assert_eq!(link["annotations"]["idempotentHint"], true);
        assert_eq!(link["annotations"]["openWorldHint"], false);
        let description = link["description"].as_str().unwrap();
        assert!(description.contains("Register every pull request you open"));
        assert!(description.contains("so zenith tracks it") && !description.contains("T3 Code"));
        // McpHttpServer.test.ts: annotated tools.
        let status = descriptors().iter().find(|tool| tool["name"] == "preview_status").unwrap();
        assert_eq!(status["annotations"]["readOnlyHint"], true);
        assert_eq!(status["annotations"]["destructiveHint"], false);
        let click = descriptors().iter().find(|tool| tool["name"] == "preview_click").unwrap();
        assert_eq!(click["annotations"]["destructiveHint"], true);
        assert_eq!(
            click["outputSchema"],
            json!({"type": "object", "properties": {"toolIcon": click["outputSchema"]["properties"]["toolIcon"].clone()}, "additionalProperties": true, "description": "The preview action completed successfully."})
        );
        let evaluate = descriptors().iter().find(|tool| tool["name"] == "preview_evaluate").unwrap();
        assert_eq!(evaluate["outputSchema"]["type"], "object");
    }

    // tools.test.ts: every preview tool takes an object with a described tabId.
    #[test]
    fn exports_provider_compatible_object_schemas_with_described_parameters() {
        for tool in descriptors()
            .iter()
            .filter(|tool| tool["name"].as_str().is_some_and(|name| name.starts_with("preview_")))
        {
            let schema = &tool["inputSchema"];
            assert!(tool["description"].as_str().unwrap().len() > 40);
            assert_eq!(schema["type"], "object");
            assert!(schema.get("anyOf").is_none() && schema.get("oneOf").is_none());
            assert!(schema["properties"].get("tabId").is_some(), "{}", tool["name"]);
        }
    }

    #[test]
    fn encodes_with_the_schema_order_and_drops_unknown_keys() {
        let schema = success_schema("preview_evaluate").unwrap();
        let encoded = encode_with_schema(
            &json!({"value": [1], "toolIcon": {"_tag": "website", "pageUrl": "http://a/"}, "extra": 1}),
            schema,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&encoded).unwrap(),
            r#"{"toolIcon":{"_tag":"website","pageUrl":"http://a/"},"value":[1]}"#
        );
        let action = success_schema("preview_scroll").unwrap();
        assert_eq!(encode_with_schema(&json!({"tabId": "tab-8"}), action), Some(json!({})));
        let status = success_schema("preview_status").unwrap();
        assert_eq!(encode_with_schema(&json!({"available": true}), status), None);
    }
}
