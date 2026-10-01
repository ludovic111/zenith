//! `toolkits/device`: `device_list`, `device_open`, `device_close` and the image tool
//! `device_screenshot`. Their parameters decode as in TypeScript and a credential without the
//! `device` capability gets the same refusal, but there is no device service in the Rust
//! server yet (plan §6.13, WP-32), so every call that gets past those checks says devices are
//! not available here.

use serde_json::{json, Map, Value};
use zc_ports::TaggedError;

use crate::errors::device_tool_unavailable;
use crate::params::{decode_arguments, optional, Field, Spec};
use crate::scope::{McpCapability, McpInvocationScope};
use crate::tools::text_content;

/// What a credential without `device` gets (`requireDeviceAccess`).
pub const DEVICE_ACCESS_OFF: &str = "Agent device access is turned off for this environment.";

/// What every device call gets until the device package exists.
pub const DEVICES_NOT_AVAILABLE: &str =
    "Devices aren't available on this server yet: simulators and emulators cannot be opened or driven through these tools. Use xcrun simctl, adb or the agent-device CLI from the shell instead.";

fn device_id() -> Spec {
    Spec::Trimmed { max: Some(256) }
}

fn host_id() -> Spec {
    Spec::Trimmed { max: Some(128) }
}

/// The parameter struct of a device tool (`DeviceTool*Input`).
pub fn fields(tool: &str) -> Vec<Field> {
    match tool {
        "device_list" => vec![optional("hostId", Spec::String)],
        "device_open" => vec![
            optional("deviceId", device_id()),
            optional("platform", Spec::Literal(&["ios", "android"])),
            optional("hostId", host_id()),
        ],
        "device_close" => vec![
            optional("deviceId", device_id()),
            optional("hostId", host_id()),
            optional("shutdown", Spec::Boolean),
        ],
        // device_screenshot: DeviceToolTargetInput
        _ => vec![optional("deviceId", device_id()), optional("hostId", host_id())],
    }
}

fn refuse(scope: &McpInvocationScope) -> TaggedError {
    if scope.has(McpCapability::Device) {
        device_tool_unavailable(DEVICES_NOT_AVAILABLE)
    } else {
        device_tool_unavailable(DEVICE_ACCESS_OFF)
    }
}

/// `device_list` / `device_open` / `device_close` (parameters already decoded).
pub fn call(_tool: &str, _input: &Map<String, Value>, scope: &McpInvocationScope) -> Result<Value, TaggedError> {
    Err(refuse(scope))
}

/// `imageToolFailure("device_screenshot", "screenshot", "Device screenshot failed.")`: only
/// the tag reaches the agent.
fn screenshot_failure(tag: &str) -> Value {
    tracing::warn!(operation = "screenshot", error_tag = tag, failure_count = 1, "device_screenshot failed");
    json!({
        "content": [text_content("Device screenshot failed.")],
        "structuredContent": {"error": {"_tag": tag, "operation": "screenshot", "failureCount": 1}},
        "isError": true,
    })
}

/// `device_screenshot`.
pub fn screenshot(arguments: Option<&Value>, scope: &McpInvocationScope) -> Value {
    if decode_arguments(&fields("device_screenshot"), arguments, false).is_err() {
        return screenshot_failure("AiError");
    }
    screenshot_failure(&refuse(scope).tag)
}

/// `agentDeviceTargetArgs`: the flags that pin every `agent-device` command to one device.
pub fn agent_device_target_args(platform: &str, device_id: &str) -> Vec<String> {
    if platform == "ios" {
        vec!["--platform".into(), "ios".into(), "--udid".into(), device_id.into()]
    } else {
        vec!["--platform".into(), "android".into(), "--serial".into(), device_id.into()]
    }
}

/// `pngDimensions`: width and height from the IHDR chunk, `(0, 0)` when it is not a PNG.
pub fn png_dimensions(png: &[u8]) -> (u32, u32) {
    if png.len() < 24 || png[..8] != [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a] || &png[12..16] != b"IHDR" {
        return (0, 0);
    }
    let read = |offset: usize| u32::from_be_bytes([png[offset], png[offset + 1], png[offset + 2], png[offset + 3]]);
    (read(16), read(20))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn scope(capabilities: &[McpCapability]) -> McpInvocationScope {
        McpInvocationScope {
            environment_id: "environment-device-test".into(),
            thread_id: "thread-device-test".into(),
            provider_session_id: "provider-session-device-test".into(),
            provider_instance_id: "codex".into(),
            capabilities: capabilities.iter().copied().collect::<HashSet<_>>(),
            issued_at: 1,
        }
    }

    // device/handlers.test.ts: "pins agent-device commands to the device by platform-specific flag"
    #[test]
    fn pins_agent_device_commands_to_the_device_by_platform_specific_flag() {
        assert_eq!(agent_device_target_args("ios", "ABCD-1234"), vec!["--platform", "ios", "--udid", "ABCD-1234"]);
        assert_eq!(
            agent_device_target_args("android", "emulator-5554"),
            vec!["--platform", "android", "--serial", "emulator-5554"]
        );
    }

    // "reads PNG dimensions from the IHDR chunk"
    #[test]
    fn reads_png_dimensions_from_the_ihdr_chunk() {
        let mut png = vec![0u8; 24];
        png[..8].copy_from_slice(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&1179u32.to_be_bytes());
        png[20..24].copy_from_slice(&2556u32.to_be_bytes());
        assert_eq!(png_dimensions(&png), (1179, 2556));
        assert_eq!(png_dimensions(&[1, 2, 3]), (0, 0));
    }

    // McpDeviceToolkit.test.ts: a credential without device access is refused, and the
    // screenshot failure carries only its tag.
    #[test]
    fn refuses_without_device_access_and_says_devices_are_unavailable_otherwise() {
        let off = call("device_list", &Map::new(), &scope(&[McpCapability::PullRequests])).unwrap_err();
        assert_eq!(off.tag, "DeviceToolUnavailableError");
        assert_eq!(off.message, DEVICE_ACCESS_OFF);
        let on = call("device_open", &Map::new(), &scope(&[McpCapability::Device])).unwrap_err();
        assert_eq!(on.message, DEVICES_NOT_AVAILABLE);
        let shot = screenshot(Some(&json!({})), &scope(&[]));
        assert_eq!(shot["content"], json!([{"type": "text", "text": "Device screenshot failed."}]));
        assert_eq!(
            shot["structuredContent"],
            json!({"error": {"_tag": "DeviceToolUnavailableError", "operation": "screenshot", "failureCount": 1}})
        );
        assert_eq!(
            screenshot(Some(&json!({"deviceId": 3})), &scope(&[]))["structuredContent"]["error"]["_tag"],
            "AiError"
        );
    }
}
