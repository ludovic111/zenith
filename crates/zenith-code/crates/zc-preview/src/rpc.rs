//! The preview RPC methods of `ws.ts` (scopes from the router's scope table):
//!
//! | Method | Kind | Payload | Success |
//! |---|---|---|---|
//! | `preview.open` | unary | `PreviewOpenInput` | `PreviewSessionSnapshot` |
//! | `preview.navigate` | unary | `PreviewNavigateInput` | `PreviewSessionSnapshot` |
//! | `preview.resize` | unary | `PreviewResizeInput` | `PreviewSessionSnapshot` |
//! | `preview.refresh` | unary | `PreviewRefreshInput` | void |
//! | `preview.close` | unary | `PreviewCloseInput` | void |
//! | `preview.list` | unary | `PreviewListInput` | `PreviewListResult` |
//! | `preview.reportStatus` | unary | `PreviewReportStatusInput` | void |
//! | `subscribePreviewEvents` | stream | `{}` | `PreviewEvent` |
//! | `subscribeDiscoveredLocalServers` | stream | `{configuredUrls?}` | `DiscoveredLocalServerList` |
//!
//! Failures are `PreviewError` (`PreviewSessionLookupError`, `PreviewInvalidUrlError`);
//! payloads that do not decode die, as the TS server's schema decoding does.

use futures::StreamExt;
use serde_json::{json, Map, Value};
use tokio_stream::wrappers::UnboundedReceiverStream;
use zc_rpc::{MethodOptions, RpcError, RpcRouterBuilder};

use crate::manager::{NavigateInput, OpenInput, PreviewError, PreviewManager, ReportStatusInput};
use crate::port_scanner::{DiscoveredLocalServer, PortDiscovery, CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS, PREVIEW_URL_MAX_LENGTH};
use crate::url::js_trim;

/// `PREVIEW_VIEWPORT_MIN_DIMENSION` / `MAX_DIMENSION` / `MAX_AREA`.
const MIN_DIMENSION: i64 = 240;
const MAX_DIMENSION: i64 = 3840;
const MAX_AREA: i64 = 3840 * 2160;

/// Preset ids a stored viewport may carry (current and legacy).
const STORED_PRESET_IDS: &[&str] = &[
    "iphone-se",
    "iphone-xr",
    "iphone-12-pro",
    "iphone-14-pro-max",
    "pixel-7",
    "samsung-galaxy-s8-plus",
    "samsung-galaxy-s20-ultra",
    "ipad-mini",
    "ipad-air",
    "ipad-pro",
    "surface-pro-7",
    "surface-duo",
    "galaxy-z-fold-5",
    "asus-zenbook-fold",
    "samsung-galaxy-a51-71",
    "nest-hub",
    "nest-hub-max",
    "desktop-1920x1080",
    "desktop-1440x900",
    "laptop-1366x768",
    "laptop-1280x800",
    "ipad-pro-11",
    "iphone-15-pro",
    "pixel-8",
    "galaxy-s24",
];

fn invalid(what: &str) -> RpcError {
    RpcError::die_text(format!("Invalid preview payload: {what}"))
}

fn failure(error: PreviewError) -> RpcError {
    RpcError::Fail(error.encoded())
}

/// A `TrimmedNonEmptyString` (optionally bounded), trimmed.
fn trimmed(value: Option<&Value>, key: &str, max: Option<usize>) -> Result<String, RpcError> {
    let text = value.and_then(Value::as_str).map(js_trim).ok_or_else(|| invalid(key))?;
    if text.is_empty() || max.is_some_and(|max| text.encode_utf16().count() > max) {
        return Err(invalid(key));
    }
    Ok(text.to_owned())
}

fn optional_trimmed(payload: &Value, key: &str, max: Option<usize>) -> Result<Option<String>, RpcError> {
    match payload.get(key) {
        None => Ok(None),
        value => trimmed(value, key, max).map(Some),
    }
}

fn thread_id(payload: &Value) -> Result<String, RpcError> {
    trimmed(payload.get("threadId"), "threadId", None)
}

fn tab_id(payload: &Value) -> Result<String, RpcError> {
    trimmed(payload.get("tabId"), "tabId", Some(128))
}

fn title(value: Option<&Value>) -> Result<String, RpcError> {
    let text = value.and_then(Value::as_str).ok_or_else(|| invalid("title"))?;
    if text.encode_utf16().count() > 512 {
        return Err(invalid("title"));
    }
    Ok(text.to_owned())
}

fn dimension(value: Option<&Value>) -> Result<i64, RpcError> {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.fract() == 0.0 && (MIN_DIMENSION as f64..=MAX_DIMENSION as f64).contains(n))
        .map(|n| n as i64)
        .ok_or_else(|| invalid("viewport dimension"))
}

/// `PreviewViewportSetting`, decoded to its schema fields.
pub fn decode_viewport(value: &Value) -> Result<Value, RpcError> {
    match value.get("_tag").and_then(Value::as_str) {
        Some("fill") => Ok(json!({"_tag": "fill"})),
        Some(tag @ ("freeform" | "preset")) => {
            let width = dimension(value.get("width"))?;
            let height = dimension(value.get("height"))?;
            if width * height > MAX_AREA {
                return Err(invalid("viewport area"));
            }
            if tag == "freeform" {
                return Ok(json!({"_tag": "freeform", "width": width, "height": height}));
            }
            let preset = value
                .get("presetId")
                .and_then(Value::as_str)
                .filter(|id| STORED_PRESET_IDS.contains(id))
                .ok_or_else(|| invalid("presetId"))?;
            Ok(json!({"_tag": "preset", "width": width, "height": height, "presetId": preset}))
        }
        _ => Err(invalid("viewport")),
    }
}

/// `PreviewNavStatus`, decoded to its schema fields.
pub fn decode_nav_status(value: &Value) -> Result<Value, RpcError> {
    let url = || trimmed(value.get("url"), "url", Some(PREVIEW_URL_MAX_LENGTH));
    match value.get("_tag").and_then(Value::as_str) {
        Some("Idle") => Ok(json!({"_tag": "Idle"})),
        Some(tag @ ("Loading" | "Success")) => Ok(json!({"_tag": tag, "url": url()?, "title": title(value.get("title"))?})),
        Some("LoadFailed") => {
            let code = value
                .get("code")
                .and_then(Value::as_f64)
                .filter(|n| n.fract() == 0.0)
                .ok_or_else(|| invalid("code"))? as i64;
            let description = value.get("description").and_then(Value::as_str).ok_or_else(|| invalid("description"))?;
            Ok(json!({"_tag": "LoadFailed", "url": url()?, "title": title(value.get("title"))?, "code": code, "description": description}))
        }
        _ => Err(invalid("navStatus")),
    }
}

fn boolean(payload: &Value, key: &str) -> Result<bool, RpcError> {
    payload.get(key).and_then(Value::as_bool).ok_or_else(|| invalid(key))
}

fn decode_open(payload: &Value) -> Result<OpenInput, RpcError> {
    let profile_id = optional_trimmed(payload, "profileId", Some(64))?;
    if profile_id.as_ref().is_some_and(|id| id.chars().any(char::is_control)) {
        return Err(invalid("profileId"));
    }
    Ok(OpenInput {
        thread_id: thread_id(payload)?,
        url: optional_trimmed(payload, "url", Some(PREVIEW_URL_MAX_LENGTH))?,
        viewport: payload.get("viewport").map(decode_viewport).transpose()?,
        profile_id,
    })
}

fn configured_urls(payload: &Value) -> Result<Vec<String>, RpcError> {
    match payload.get("configuredUrls") {
        None => Ok(Vec::new()),
        Some(Value::Array(urls)) if urls.len() <= CONFIGURED_LOCAL_SERVER_URLS_MAX_ITEMS => urls
            .iter()
            .map(|url| trimmed(Some(url), "configuredUrls", Some(PREVIEW_URL_MAX_LENGTH)))
            .collect(),
        Some(_) => Err(invalid("configuredUrls")),
    }
}

/// `DiscoveredLocalServerList`.
pub fn server_list(servers: &[DiscoveredLocalServer]) -> Value {
    let mut list = Map::new();
    list.insert("servers".into(), Value::Array(servers.iter().map(DiscoveredLocalServer::to_json).collect()));
    list.insert("scannedAt".into(), json!(zc_core::time::now_iso()));
    list.insert("configuredUrlProbing".into(), json!(true));
    Value::Object(list)
}

/// Registers the nine methods.
pub fn register(builder: RpcRouterBuilder, manager: PreviewManager, discovery: PortDiscovery) -> RpcRouterBuilder {
    let options = MethodOptions::default;
    let m = manager.clone();
    let builder = builder.unary_with("preview.open", options(), move |_ctx, payload| {
        let manager = m.clone();
        async move { manager.open(decode_open(&payload)?).map_err(failure) }
    });
    let m = manager.clone();
    let builder = builder.unary_with("preview.navigate", options(), move |_ctx, payload| {
        let manager = m.clone();
        async move {
            let input = NavigateInput {
                thread_id: thread_id(&payload)?,
                tab_id: tab_id(&payload)?,
                url: trimmed(payload.get("url"), "url", Some(PREVIEW_URL_MAX_LENGTH))?,
                resolved_title: payload.get("resolvedTitle").map(|value| title(Some(value))).transpose()?,
            };
            manager.navigate(input).map_err(failure)
        }
    });
    let m = manager.clone();
    let builder = builder.unary_with("preview.resize", options(), move |_ctx, payload| {
        let manager = m.clone();
        async move {
            let viewport = decode_viewport(payload.get("viewport").ok_or_else(|| invalid("viewport"))?)?;
            manager.resize(&thread_id(&payload)?, &tab_id(&payload)?, viewport).map_err(failure)
        }
    });
    let m = manager.clone();
    let builder = builder.unary_with("preview.refresh", options(), move |_ctx, payload| {
        let manager = m.clone();
        async move {
            manager.refresh(&thread_id(&payload)?, &tab_id(&payload)?).map_err(failure)?;
            Ok(Value::Null)
        }
    });
    let m = manager.clone();
    let builder = builder.unary_with("preview.close", options(), move |_ctx, payload| {
        let manager = m.clone();
        async move {
            let tab = optional_trimmed(&payload, "tabId", Some(128))?;
            manager.close(&thread_id(&payload)?, tab.as_deref());
            Ok(Value::Null)
        }
    });
    let m = manager.clone();
    let builder = builder.unary_with("preview.list", options(), move |_ctx, payload| {
        let manager = m.clone();
        async move { Ok(manager.list(&thread_id(&payload)?)) }
    });
    let m = manager.clone();
    let builder = builder.unary_with("preview.reportStatus", options(), move |_ctx, payload| {
        let manager = m.clone();
        async move {
            let input = ReportStatusInput {
                thread_id: thread_id(&payload)?,
                tab_id: tab_id(&payload)?,
                nav_status: decode_nav_status(payload.get("navStatus").ok_or_else(|| invalid("navStatus"))?)?,
                can_go_back: boolean(&payload, "canGoBack")?,
                can_go_forward: boolean(&payload, "canGoForward")?,
            };
            manager.report_status(input).map_err(failure)?;
            Ok(Value::Null)
        }
    });
    let m = manager;
    let builder = builder.stream_with("subscribePreviewEvents", options(), move |_ctx, _payload| {
        let manager = m.clone();
        async move { Ok(UnboundedReceiverStream::new(manager.subscribe()).map(Ok::<_, RpcError>)) }
    });
    builder.stream_with("subscribeDiscoveredLocalServers", options(), move |_ctx, payload| {
        let discovery = discovery.clone();
        async move {
            let configured = configured_urls(&payload)?;
            let retain = discovery.retain().await;
            let initial = discovery.scan(&configured).await;
            let first = server_list(&initial);
            let (subscription, changes) = discovery.subscribe(&configured, initial);
            let rest = UnboundedReceiverStream::new(changes).map(move |servers| {
                // The subscription and the retention last as long as the stream.
                let _held = (&retain, &subscription);
                Ok::<_, RpcError>(server_list(&servers))
            });
            Ok(futures::stream::once(async move { Ok(first) }).chain(rest))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_viewports_and_nav_statuses_to_their_schema_fields() {
        assert_eq!(decode_viewport(&json!({"_tag": "fill", "extra": 1})).unwrap(), json!({"_tag": "fill"}));
        assert_eq!(
            decode_viewport(&json!({"_tag": "preset", "presetId": "pixel-8", "width": 412, "height": 915})).unwrap(),
            json!({"_tag": "preset", "width": 412, "height": 915, "presetId": "pixel-8"})
        );
        assert!(decode_viewport(&json!({"_tag": "freeform", "width": 100, "height": 300})).is_err());
        assert!(decode_viewport(&json!({"_tag": "freeform", "width": 3840, "height": 3840})).is_err());
        assert_eq!(
            decode_nav_status(&json!({"_tag": "Success", "url": " http://a/ ", "title": "A"})).unwrap(),
            json!({"_tag": "Success", "url": "http://a/", "title": "A"})
        );
        assert!(decode_nav_status(&json!({"_tag": "LoadFailed", "url": "http://a/", "title": ""})).is_err());
    }
}
