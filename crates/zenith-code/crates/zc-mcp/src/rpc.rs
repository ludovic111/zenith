//! The `previewAutomation.*` RPC methods of `ws.ts` (all need `orchestration:operate`, from
//! the router's scope table):
//!
//! | Method | Kind | Payload | Success |
//! |---|---|---|---|
//! | `previewAutomation.connect` | stream | `PreviewAutomationHost` | `PreviewAutomationStreamEvent` |
//! | `previewAutomation.respond` | unary | `PreviewAutomationResponse` | void |
//! | `previewAutomation.focusHost` | unary | `PreviewAutomationHostFocus` | void |

use futures::StreamExt;
use serde_json::Value;
use zc_rpc::{MethodOptions, RpcError, RpcRouterBuilder};

use crate::broker::{
    LiveTab, PreviewAutomationBroker, PreviewAutomationHost, PreviewAutomationHostFocus, PreviewAutomationResponse, PREVIEW_AUTOMATION_OPERATIONS,
};
use crate::params::{js_length, js_trim};

fn invalid(field: &str) -> RpcError {
    RpcError::die_text(format!("Invalid previewAutomation payload: {field}"))
}

/// A `TrimmedNonEmptyString` with a maximum length.
fn trimmed(payload: &Value, key: &str, max: Option<usize>) -> Result<String, RpcError> {
    let text = payload.get(key).and_then(Value::as_str).map(js_trim).ok_or_else(|| invalid(key))?;
    if text.is_empty() || max.is_some_and(|max| js_length(text) > max) {
        return Err(invalid(key));
    }
    Ok(text.to_owned())
}

fn decode_host(payload: &Value) -> Result<PreviewAutomationHost, RpcError> {
    let supported_operations = match payload.get("supportedOperations") {
        None => None,
        Some(Value::Array(operations)) => Some(
            operations
                .iter()
                .map(|operation| {
                    operation
                        .as_str()
                        .filter(|operation| PREVIEW_AUTOMATION_OPERATIONS.contains(operation))
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("supportedOperations"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => return Err(invalid("supportedOperations")),
    };
    Ok(PreviewAutomationHost {
        client_id: trimmed(payload, "clientId", Some(128))?,
        environment_id: trimmed(payload, "environmentId", None)?,
        supported_operations,
    })
}

fn decode_focus(payload: &Value) -> Result<PreviewAutomationHostFocus, RpcError> {
    let live_tabs = match payload.get("liveTabs") {
        None => None,
        Some(Value::Array(tabs)) => Some(
            tabs.iter()
                .map(|tab| {
                    Ok(LiveTab {
                        thread_id: trimmed(tab, "threadId", None)?,
                        tab_id: trimmed(tab, "tabId", Some(128))?,
                        visible: match tab.get("visible") {
                            None => None,
                            Some(Value::Bool(visible)) => Some(*visible),
                            Some(_) => return Err(invalid("liveTabs.visible")),
                        },
                    })
                })
                .collect::<Result<Vec<_>, RpcError>>()?,
        ),
        Some(_) => return Err(invalid("liveTabs")),
    };
    Ok(PreviewAutomationHostFocus {
        client_id: trimmed(payload, "clientId", Some(128))?,
        environment_id: trimmed(payload, "environmentId", None)?,
        connection_id: trimmed(payload, "connectionId", Some(64))?,
        focused: payload.get("focused").and_then(Value::as_bool).ok_or_else(|| invalid("focused"))?,
        live_tabs,
    })
}

fn decode_response(payload: &Value) -> Result<PreviewAutomationResponse, RpcError> {
    let error = match payload.get("error") {
        None => None,
        Some(error @ Value::Object(fields)) => {
            let tag_ok = fields.get("_tag").and_then(Value::as_str).is_some_and(|tag| !js_trim(tag).is_empty());
            if !tag_ok || !fields.get("message").is_some_and(Value::is_string) {
                return Err(invalid("error"));
            }
            Some(error.clone())
        }
        Some(_) => return Err(invalid("error")),
    };
    Ok(PreviewAutomationResponse {
        client_id: trimmed(payload, "clientId", Some(128))?,
        connection_id: trimmed(payload, "connectionId", Some(64))?,
        request_id: trimmed(payload, "requestId", None)?,
        ok: payload.get("ok").and_then(Value::as_bool).ok_or_else(|| invalid("ok"))?,
        result: payload.get("result").cloned(),
        error,
    })
}

/// Registers the three methods.
pub fn register(builder: RpcRouterBuilder, broker: PreviewAutomationBroker) -> RpcRouterBuilder {
    let connect = broker.clone();
    let builder = builder.stream_with("previewAutomation.connect", MethodOptions::default(), move |_ctx, payload| {
        let broker = connect.clone();
        async move {
            let host = decode_host(&payload)?;
            Ok(broker.connect(host).map(Ok::<_, RpcError>))
        }
    });
    let respond = broker.clone();
    let builder = builder.unary_with("previewAutomation.respond", MethodOptions::default(), move |_ctx, payload| {
        let broker = respond.clone();
        async move {
            broker.respond(decode_response(&payload)?);
            Ok(Value::Null)
        }
    });
    builder.unary_with("previewAutomation.focusHost", MethodOptions::default(), move |_ctx, payload| {
        let broker = broker.clone();
        async move {
            broker.focus_host(decode_focus(&payload)?);
            Ok(Value::Null)
        }
    })
}
