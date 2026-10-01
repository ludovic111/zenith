//! `toolkits/preview`: every `preview_*` tool but the snapshot, through the
//! [`crate::PreviewAutomationBroker`]. Actions that do not report page state are followed by a
//! short `status` read so the result carries the page's site icon (`toolIcon`).

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};
use zc_ports::TaggedError;

use crate::broker::PreviewAutomationInvokeInput;
use crate::errors;
use crate::params::{optional, required, Bound, Field, Spec};
use crate::scope::{require_capability, McpCapability, McpInvocationScope};
use crate::tools::McpServices;

/// `PREVIEW_RECORDING_STOP_TIMEOUT_MS`.
pub const PREVIEW_RECORDING_STOP_TIMEOUT_MS: u64 = 120_000;
/// `PROVIDER_SEND_TURN_MAX_FILE_BYTES`.
pub const PROVIDER_SEND_TURN_MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;

/// `PREVIEW_VIEWPORT_PRESET_IDS`.
pub const PREVIEW_VIEWPORT_PRESET_IDS: &[&str] = &[
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
];

/// `PREVIEW_VIEWPORT_MAX_AREA`.
pub const PREVIEW_VIEWPORT_MAX_AREA: i64 = 3840 * 2160;

const PREVIEW_TOOLS: &[&str] = &[
    "preview_status",
    "preview_open",
    "preview_navigate",
    "preview_resize",
    "preview_set_appearance",
    "preview_click",
    "preview_type",
    "preview_press",
    "preview_scroll",
    "preview_evaluate",
    "preview_wait_for",
    "preview_recording_start",
    "preview_recording_stop",
];

pub fn is_preview_tool(name: &str) -> bool {
    PREVIEW_TOOLS.contains(&name)
}

fn tab_id() -> Field {
    optional("tabId", Spec::Trimmed { max: Some(128) })
}

fn bounded_url() -> Spec {
    Spec::CheckedTrimmed { max: Some(2048) }
}

fn timeout_ms() -> Field {
    optional(
        "timeoutMs",
        Spec::Int {
            min: Some(Bound::Exclusive(0.0)),
            max: Some(Bound::Inclusive(60_000.0)),
            between: false,
        },
    )
}

fn selector() -> Spec {
    Spec::Trimmed { max: None }
}

fn viewport_dimension() -> Spec {
    Spec::Int {
        min: Some(Bound::Inclusive(240.0)),
        max: Some(Bound::Inclusive(3840.0)),
        between: true,
    }
}

/// The parameter struct of a preview tool (`PreviewAutomation*Input`), fields in schema order.
pub fn fields(tool: &str) -> Vec<Field> {
    match tool {
        "preview_open" => vec![
            tab_id(),
            optional("url", bounded_url()),
            optional("open", Spec::Boolean),
            optional("show", Spec::Boolean),
            optional("reuseExistingTab", Spec::Boolean),
        ],
        "preview_navigate" => vec![
            tab_id(),
            optional("url", bounded_url()),
            optional(
                "target",
                Spec::Tagged {
                    key: "kind",
                    members: vec![
                        (
                            "url",
                            Spec::Struct(vec![required("kind", Spec::Literal(&["url"])), required("url", bounded_url())]),
                        ),
                        (
                            "environment-port",
                            Spec::Struct(vec![
                                required("kind", Spec::Literal(&["environment-port"])),
                                required(
                                    "port",
                                    Spec::Int {
                                        min: Some(Bound::Exclusive(0.0)),
                                        max: Some(Bound::Exclusive(65_536.0)),
                                        between: false,
                                    },
                                ),
                                optional("protocol", Spec::Literal(&["http", "https"])),
                                optional("path", Spec::String),
                            ]),
                        ),
                    ],
                    expected: "{ readonly \"kind\": \"url\", ... } | { readonly \"kind\": \"environment-port\", ... }",
                },
            ),
            optional("readiness", Spec::Literal(&["load", "domContentLoaded", "none"])),
            timeout_ms(),
        ],
        "preview_resize" => vec![
            tab_id(),
            required("mode", Spec::Literal(&["fill", "freeform", "preset"])),
            optional("preset", Spec::Literal(PREVIEW_VIEWPORT_PRESET_IDS)),
            optional("width", viewport_dimension()),
            optional("height", viewport_dimension()),
            optional("orientation", Spec::Literal(&["portrait", "landscape"])),
            timeout_ms(),
        ],
        "preview_set_appearance" => vec![tab_id(), required("colorScheme", Spec::Literal(&["system", "light", "dark"]))],
        "preview_click" => vec![
            tab_id(),
            optional("selector", selector()),
            optional("locator", selector()),
            optional("x", Spec::Number),
            optional("y", Spec::Number),
            timeout_ms(),
        ],
        "preview_type" => vec![
            tab_id(),
            required("text", Spec::String),
            optional("selector", selector()),
            optional("locator", selector()),
            optional("clear", Spec::Boolean),
            timeout_ms(),
        ],
        "preview_press" => vec![
            tab_id(),
            required("key", Spec::CheckedTrimmed { max: None }),
            optional("modifiers", Spec::Array(Box::new(Spec::Literal(&["Alt", "Control", "Meta", "Shift"])))),
        ],
        "preview_scroll" => vec![
            tab_id(),
            optional("deltaX", Spec::Number),
            optional("deltaY", Spec::Number),
            optional("selector", selector()),
            optional("locator", selector()),
        ],
        "preview_evaluate" => vec![
            tab_id(),
            required("expression", Spec::CheckedTrimmed { max: Some(64_000) }),
            optional("awaitPromise", Spec::Boolean),
            optional("returnByValue", Spec::Boolean),
        ],
        "preview_wait_for" => vec![
            tab_id(),
            optional("selector", selector()),
            optional("locator", selector()),
            optional("text", Spec::Trimmed { max: None }),
            optional("urlIncludes", Spec::Trimmed { max: None }),
            timeout_ms(),
        ],
        // preview_status, preview_recording_start, preview_recording_stop
        _ => vec![tab_id()],
    }
}

/// The schemas' cross-field filters (`Schema.makeFilter`), on the decoded input.
pub fn check(tool: &str, input: &Map<String, Value>) -> Option<String> {
    let has = |key: &str| input.contains_key(key);
    match tool {
        "preview_open" => (has("tabId") && input.get("reuseExistingTab") == Some(&Value::Bool(false)))
            .then(|| "tabId cannot be combined with reuseExistingTab=false.".to_owned()),
        "preview_navigate" => (usize::from(has("url")) + usize::from(has("target")) != 1).then(|| "Provide exactly one of url or target.".to_owned()),
        "preview_resize" => {
            let (has_preset, has_width, has_height) = (has("preset"), has("width"), has("height"));
            if has_width != has_height {
                return Some("Custom dimensions require both width and height.".into());
            }
            match input.get("mode").and_then(Value::as_str) {
                Some("fill") => {
                    return (has_preset || has_width || has("orientation"))
                        .then(|| "Fill mode does not accept a preset, dimensions, or orientation.".to_owned());
                }
                Some("freeform") => {
                    if !has_width || !has_height || has_preset || has("orientation") {
                        return Some("Freeform mode requires width and height and does not accept a preset or orientation.".into());
                    }
                }
                _ => {
                    if !has_preset || has_width || has_height {
                        return Some("Preset mode requires a preset and does not accept custom dimensions.".into());
                    }
                }
            }
            let dimension = |key: &str| input.get(key).and_then(Value::as_i64);
            match (dimension("width"), dimension("height")) {
                (Some(width), Some(height)) if width * height > PREVIEW_VIEWPORT_MAX_AREA => {
                    Some(format!("Custom viewport area must not exceed {PREVIEW_VIEWPORT_MAX_AREA} pixels."))
                }
                _ => None,
            }
        }
        "preview_click" => {
            let selector_modes = usize::from(has("selector")) + usize::from(has("locator"));
            if has("x") != has("y") {
                return Some("Coordinates require both x and y.".into());
            }
            let coordinate_modes = usize::from(has("x") && has("y"));
            (selector_modes + coordinate_modes != 1).then(|| "Provide exactly one click target.".to_owned())
        }
        "preview_type" => (has("selector") && has("locator")).then(|| "Provide at most one of selector or locator.".to_owned()),
        "preview_scroll" => {
            if has("selector") && has("locator") {
                return Some("Provide at most one of selector or locator.".into());
            }
            (!has("deltaX") && !has("deltaY")).then(|| "Provide deltaX or deltaY.".to_owned())
        }
        "preview_wait_for" => {
            if has("selector") && has("locator") {
                return Some("Provide at most one of selector or locator.".into());
            }
            (!has("selector") && !has("locator") && !has("text") && !has("urlIncludes")).then(|| "Provide at least one wait condition.".to_owned())
        }
        _ => None,
    }
}

/// `normalizePreviewOpenInput`: `show` collapses onto `open`, tab reuse defaults on, and an
/// unstated `open` stays unstated (the desktop's own preference decides).
pub fn normalize_preview_open_input(mut input: Map<String, Value>) -> Map<String, Value> {
    let open = input.get("open").or_else(|| input.get("show")).cloned();
    if let Some(open) = open {
        input.insert("open".into(), open.clone());
        input.insert("show".into(), open);
    }
    let reuse = input.get("reuseExistingTab").cloned().unwrap_or(Value::Bool(true));
    input.insert("reuseExistingTab".into(), reuse);
    input
}

/// `{_tag:"website", pageUrl}` for an http(s) page.
fn tool_icon(page: &Value) -> Option<Value> {
    static HTTP: OnceLock<Regex> = OnceLock::new();
    let url = page.get("url").and_then(Value::as_str)?;
    let http = HTTP.get_or_init(|| Regex::new("(?i)^https?://").expect("static regex"));
    (http.is_match(url) && url.encode_utf16().count() <= 4096).then(|| json!({"_tag": "website", "pageUrl": url}))
}

/// The handlers.
pub struct PreviewTools<'a> {
    pub services: &'a McpServices,
}

impl PreviewTools<'_> {
    /// `invoke`: one broker call, plus the page status for the site icon after an action.
    pub async fn invoke(
        &self,
        scope: &McpInvocationScope,
        operation: &str,
        input: Value,
        timeout_ms: Option<u64>,
        tab_id: Option<String>,
    ) -> Result<(Value, Option<Value>), TaggedError> {
        let scope = require_capability(scope, McpCapability::Preview)?;
        let broker = &self.services.broker;
        let mut request = PreviewAutomationInvokeInput::new(scope.clone(), operation, input);
        request.timeout_ms = timeout_ms;
        request.tab_id = tab_id.clone();
        let outcome = broker.invoke(request).await;
        let target_tab = match outcome.routed_tab {
            Some(routed) => routed,
            None => tab_id,
        };
        let result = outcome.result?;
        if matches!(operation, "status" | "open" | "navigate" | "snapshot") {
            return Ok((result, None));
        }
        let result_tab = if operation != "evaluate" {
            result.get("tabId").and_then(Value::as_str).map(str::to_owned)
        } else {
            None
        };
        let status_tab = result_tab.or(target_tab);
        let mut status = PreviewAutomationInvokeInput::new(scope.clone(), "status", json!({}));
        status.timeout_ms = Some(500);
        status.update_current_tab = false;
        status.tab_id = status_tab;
        let page = broker.invoke(status).await.result.ok();
        Ok((result, page.as_ref().and_then(tool_icon)))
    }

    /// `invokeTargeted`: `tabId` routes the call; the rest is the operation's input; the
    /// result is spread with the site icon.
    pub async fn invoke_targeted(
        &self,
        scope: &McpInvocationScope,
        operation: &str,
        mut input: Map<String, Value>,
        timeout_ms: Option<u64>,
    ) -> Result<Value, TaggedError> {
        let tab_id = input.shift_remove("tabId").and_then(|tab| tab.as_str().map(str::to_owned));
        let (result, icon) = self.invoke(scope, operation, Value::Object(input), timeout_ms, tab_id).await?;
        let mut out = result.as_object().cloned().unwrap_or_default();
        if let Some(icon) = icon {
            out.insert("toolIcon".into(), icon);
        }
        Ok(Value::Object(out))
    }

    pub async fn call(&self, tool: &str, input: Map<String, Value>, scope: &McpInvocationScope) -> Result<Value, TaggedError> {
        let timeout = |input: &Map<String, Value>| input.get("timeoutMs").and_then(Value::as_u64);
        match tool {
            "preview_status" => self.invoke_targeted(scope, "status", input, None).await,
            "preview_open" => self.invoke_targeted(scope, "open", normalize_preview_open_input(input), None).await,
            "preview_navigate" => {
                let timeout = timeout(&input);
                self.invoke_targeted(scope, "navigate", input, timeout).await
            }
            "preview_resize" => {
                let timeout = timeout(&input);
                self.invoke_targeted(scope, "resize", input, timeout).await
            }
            "preview_set_appearance" => self.invoke_targeted(scope, "setColorScheme", input, None).await,
            "preview_click" => {
                let timeout = timeout(&input);
                self.invoke_targeted(scope, "click", input, timeout).await
            }
            "preview_type" => {
                let timeout = timeout(&input);
                self.invoke_targeted(scope, "type", input, timeout).await
            }
            "preview_press" => self.invoke_targeted(scope, "press", input, None).await,
            "preview_scroll" => self.invoke_targeted(scope, "scroll", input, None).await,
            "preview_evaluate" => {
                let mut input = input;
                let tab_id = input.shift_remove("tabId").and_then(|tab| tab.as_str().map(str::to_owned));
                let (result, icon) = self.invoke(scope, "evaluate", Value::Object(input), None, tab_id).await?;
                let mut out = Map::new();
                out.insert("value".into(), result);
                if let Some(icon) = icon {
                    out.insert("toolIcon".into(), icon);
                }
                Ok(Value::Object(out))
            }
            "preview_wait_for" => {
                let timeout = timeout(&input);
                self.invoke_targeted(scope, "waitFor", input, timeout).await
            }
            "preview_recording_start" => self.invoke_targeted(scope, "recordingStart", input, None).await,
            "preview_recording_stop" => {
                let scope = require_capability(scope, McpCapability::Preview)?;
                let mut input = input;
                let tab_id = input.shift_remove("tabId").and_then(|tab| tab.as_str().map(str::to_owned));
                input.insert("transferToEnvironment".into(), json!(true));
                let (result, icon) = self
                    .invoke(scope, "recordingStop", Value::Object(input), Some(PREVIEW_RECORDING_STOP_TIMEOUT_MS), tab_id)
                    .await?;
                let mut artifact = claim_preview_recording(&self.services.attachments_dir, &scope.thread_id, &result)?;
                if let Some(icon) = icon {
                    artifact.insert("toolIcon".into(), icon);
                }
                Ok(Value::Object(artifact))
            }
            _ => Err(TaggedError::new("PreviewAutomationExecutionError", "unknown preview tool")),
        }
    }
}

fn attachment_id_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new("(?i)^([a-z0-9_]+(?:-[a-z0-9_]+)*)-([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:-([a-z0-9]{1,10}))?$")
            .expect("static regex")
    })
}

/// `parseAttachmentUuid` (group 2) / `parseAttachmentFileExtension` (group 3).
fn attachment_id_part(attachment_id: &str, group: usize) -> Option<String> {
    let normalized = zc_projections::attachments::normalize_attachment_relative_path(attachment_id)?;
    if normalized.contains('/') || normalized.contains('.') {
        return None;
    }
    let captures = attachment_id_pattern().captures(&normalized)?;
    Some(captures.get(group)?.as_str().to_lowercase())
}

/// Decodes `UploadedRecordingArtifact` (the recording artifact plus `uploadedAttachmentId?`),
/// fields in schema order.
fn decode_uploaded_artifact(response: &Value) -> Option<(Map<String, Value>, Option<String>)> {
    let object = response.as_object()?;
    let mut artifact = Map::new();
    for key in ["id", "tabId", "path", "mimeType", "sizeBytes", "createdAt"] {
        let value = object.get(key)?;
        let valid = match key {
            "sizeBytes" => value.as_f64().is_some_and(|n| value.is_number() && n.fract() == 0.0),
            "tabId" => value.as_str().is_some_and(|tab| {
                let trimmed = crate::params::js_trim(tab);
                !trimmed.is_empty() && trimmed.encode_utf16().count() <= 128
            }),
            _ => value.is_string(),
        };
        if !valid {
            return None;
        }
        let value = match (key, value.as_str()) {
            ("tabId", Some(tab)) => json!(crate::params::js_trim(tab)),
            _ => value.clone(),
        };
        artifact.insert(key.into(), value);
    }
    let uploaded = match object.get("uploadedAttachmentId") {
        None => None,
        Some(Value::String(id)) => Some(id.clone()),
        Some(_) => return None,
    };
    Some((artifact, uploaded))
}

fn valid_recording_file(path: &Path, size_bytes: u64) -> Result<(), std::io::Error> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_file() && metadata.len() == size_bytes && size_bytes > 0 && size_bytes <= PROVIDER_SEND_TURN_MAX_FILE_BYTES {
        Ok(())
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "recording file does not match"))
    }
}

/// `claimPreviewRecording`: moves the recording the desktop uploaded as a pending attachment
/// into the thread's attachments and returns the artifact with its environment-local path.
/// Overlapping claims of the same upload return the same retained file.
pub fn claim_preview_recording(attachments_dir: &Path, thread_id: &str, response: &Value) -> Result<Map<String, Value>, TaggedError> {
    let transfer = |cause: Option<&str>| errors::recording_transfer(thread_id, cause.map(|cause| json!(cause)).as_ref());
    let Some((artifact, uploaded)) = decode_uploaded_artifact(response) else {
        return Err(transfer(Some("malformed recording artifact")));
    };
    let Some(uploaded) = uploaded.filter(|id| !id.is_empty()) else {
        return Err(errors::recording_desktop_update_required(thread_id, None));
    };
    let uuid = attachment_id_part(&uploaded, 2);
    let extension = attachment_id_part(&uploaded, 3);
    let segment = zc_projections::attachments::to_safe_thread_attachment_segment(thread_id);
    let (Some(uuid), Some(extension), Some(segment)) = (uuid, extension, segment) else {
        return Err(transfer(None));
    };
    let pending_id = format!("pending-{uuid}-{extension}");
    if uploaded != pending_id {
        return Err(transfer(None));
    }
    // The same completed upload can be returned to overlapping stop requests.
    let final_id = format!("{segment}-{uuid}-{extension}");
    let current = zc_providers::attachments::resolve_attachment_relative_path(attachments_dir, &format!("{pending_id}.{extension}"));
    let target = zc_providers::attachments::resolve_attachment_relative_path(attachments_dir, &format!("{final_id}.{extension}"));
    let (Some(current), Some(target)) = (current, target) else {
        return Err(transfer(None));
    };
    let size_bytes = artifact.get("sizeBytes").and_then(Value::as_u64).unwrap_or(0);
    let moved = valid_recording_file(&current, size_bytes).and_then(|()| std::fs::rename(&current, &target));
    match moved {
        Ok(()) => {}
        // Another stop may already have claimed this exact upload for this thread.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            valid_recording_file(&target, size_bytes).map_err(|error| transfer(Some(&error.to_string())))?;
        }
        Err(error) => return Err(transfer(Some(&error.to_string()))),
    }
    let mut recording = artifact;
    recording.insert("id".into(), json!(final_id));
    recording.insert("path".into(), json!(target.to_string_lossy()));
    Ok(recording)
}

#[cfg(test)]
mod tests {
    use super::*;

    // handlers.test.ts: normalizePreviewOpenInput
    #[test]
    fn leaves_an_unstated_visibility_for_the_client_preference_to_decide() {
        assert_eq!(Value::Object(normalize_preview_open_input(Map::new())), json!({"reuseExistingTab": true}));
    }

    #[test]
    fn preserves_an_explicit_background_only_opt_out() {
        let input = json!({"open": false}).as_object().unwrap().clone();
        assert_eq!(
            Value::Object(normalize_preview_open_input(input)),
            json!({"open": false, "reuseExistingTab": true, "show": false})
        );
    }

    #[test]
    fn supports_show_as_a_legacy_alias_while_preferring_open() {
        let show = json!({"show": false}).as_object().unwrap().clone();
        assert_eq!(
            Value::Object(normalize_preview_open_input(show)),
            json!({"open": false, "reuseExistingTab": true, "show": false})
        );
        let both = json!({"open": true, "show": false}).as_object().unwrap().clone();
        assert_eq!(
            Value::Object(normalize_preview_open_input(both)),
            json!({"open": true, "reuseExistingTab": true, "show": true})
        );
    }

    fn pending_upload(dir: &Path, contents: &str) -> String {
        std::fs::create_dir_all(dir).unwrap();
        let id = format!("pending-{}-webm", zc_core::ids::uuid_v4());
        std::fs::write(dir.join(format!("{id}.webm")), contents).unwrap();
        id
    }

    fn response(uploaded: Option<&str>, size: u64) -> Value {
        let mut response = json!({
            "id": "desktop-recording", "tabId": "tab-1", "path": "/desktop/recording.webm",
            "mimeType": "video/webm", "sizeBytes": size, "createdAt": "2026-09-07T00:00:00.000Z",
        });
        if let Some(uploaded) = uploaded {
            response["uploadedAttachmentId"] = json!(uploaded);
        }
        response
    }

    // "overlapping and repeated claims return the same retained recording"
    #[test]
    fn overlapping_and_repeated_claims_return_the_same_retained_recording() {
        let dir = tempfile::tempdir().unwrap();
        let attachments = dir.path().join("attachments");
        let uploaded = pending_upload(&attachments, "video!");
        let response = response(Some(&uploaded), 6);
        let first = claim_preview_recording(&attachments, "thread-1", &response).unwrap();
        let second = claim_preview_recording(&attachments, "thread-1", &response).unwrap();
        assert_eq!(first, second);
        let path = first["path"].as_str().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "video!");
        assert!(!attachments.join(format!("{uploaded}.webm")).exists());
        assert_eq!(
            zc_projections::attachments::parse_thread_segment_from_attachment_id(first["id"].as_str().unwrap()).as_deref(),
            Some("thread-1")
        );
        assert_eq!(
            first.keys().collect::<Vec<_>>(),
            vec!["id", "tabId", "path", "mimeType", "sizeBytes", "createdAt"]
        );
        assert!(claim_preview_recording(&attachments, "thread-2", &response).is_err());
        let wrong_path = self::response(Some(&format!("../{uploaded}")), 6);
        assert!(claim_preview_recording(&attachments, "thread-1", &wrong_path).is_err());
    }

    // "claims only a complete uploaded recording (reported bytes: 6 / 5)"
    #[test]
    fn claims_only_a_complete_uploaded_recording() {
        for size in [6, 5] {
            let dir = tempfile::tempdir().unwrap();
            let attachments = dir.path().join("attachments");
            let uploaded = pending_upload(&attachments, "video!");
            let result = claim_preview_recording(&attachments, "thread-1", &response(Some(&uploaded), size));
            if size == 6 {
                let recording = result.unwrap();
                assert_ne!(recording["path"], "/desktop/recording.webm");
                assert!(!attachments.join(format!("{uploaded}.webm")).exists());
            } else {
                assert_eq!(result.unwrap_err().tag, "PreviewAutomationRecordingTransferError");
                assert!(attachments.join(format!("{uploaded}.webm")).exists());
            }
        }
    }

    // "reports an older desktop without returning its inaccessible path"
    #[test]
    fn reports_an_older_desktop_without_returning_its_inaccessible_path() {
        let dir = tempfile::tempdir().unwrap();
        let error = claim_preview_recording(dir.path(), "thread-1", &response(None, 6)).unwrap_err();
        assert_eq!(error.tag, "PreviewAutomationRecordingDesktopUpdateRequiredError");
        assert!(error.message.contains("Update the desktop app"));
    }

    #[test]
    fn applies_the_cross_field_filters() {
        let check_of = |tool: &str, value: Value| check(tool, value.as_object().unwrap());
        assert_eq!(check_of("preview_click", json!({"x": 1})).as_deref(), Some("Coordinates require both x and y."));
        assert_eq!(check_of("preview_click", json!({})).as_deref(), Some("Provide exactly one click target."));
        assert_eq!(check_of("preview_click", json!({"locator": "a"})), None);
        assert_eq!(
            check_of("preview_resize", json!({"mode": "freeform", "width": 3840, "height": 3840})).as_deref(),
            Some("Custom viewport area must not exceed 8294400 pixels.")
        );
        assert_eq!(
            check_of("preview_resize", json!({"mode": "preset"})).as_deref(),
            Some("Preset mode requires a preset and does not accept custom dimensions.")
        );
        assert_eq!(
            check_of("preview_resize", json!({"mode": "freeform", "width": 300})).as_deref(),
            Some("Custom dimensions require both width and height.")
        );
        assert_eq!(check_of("preview_scroll", json!({})).as_deref(), Some("Provide deltaX or deltaY."));
        assert_eq!(check_of("preview_wait_for", json!({})).as_deref(), Some("Provide at least one wait condition."));
        assert_eq!(
            check_of("preview_type", json!({"text": "a", "selector": "a", "locator": "b"})).as_deref(),
            Some("Provide at most one of selector or locator.")
        );
    }
}
