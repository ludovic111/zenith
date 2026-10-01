//! `deriveAuthClientMetadata` (`auth/utils.ts`): what the server records about a client when
//! it pairs: user agent, IP, and a device type / OS / browser guessed from the user agent.

use std::sync::LazyLock;

use regex::Regex;
use zc_contracts::{AuthClientMetadata, AuthClientMetadataDeviceType as DeviceType};

use crate::token::js_trim;

/// What a client said about itself (`/oauth/token`'s `client_label`, `client_device_type`,
/// `client_os`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PresentedClient {
    pub label: Option<String>,
    pub device_type: Option<DeviceType>,
    pub os: Option<String>,
}

fn non_empty_trimmed(value: Option<&str>) -> Option<String> {
    value.map(js_trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

/// `normalizeIpAddress`: trimmed, `::ffff:` prefix of IPv4-mapped addresses dropped.
pub fn normalize_ip_address(value: Option<&str>) -> Option<String> {
    let normalized = non_empty_trimmed(value)?;
    Some(match normalized.strip_prefix("::ffff:") {
        Some(rest) => rest.to_owned(),
        None => normalized,
    })
}

static BOT: LazyLock<Regex> = LazyLock::new(|| Regex::new("bot|crawler|spider|slurp|curl|wget").unwrap());
static TABLET: LazyLock<Regex> = LazyLock::new(|| Regex::new("ipad|tablet").unwrap());
static MOBILE: LazyLock<Regex> = LazyLock::new(|| Regex::new("iphone|android.+mobile|mobile").unwrap());

fn infer_device_type(user_agent: Option<&str>) -> DeviceType {
    let Some(user_agent) = user_agent else {
        return DeviceType::Unknown;
    };
    let normalized = user_agent.to_lowercase();
    if BOT.is_match(&normalized) {
        DeviceType::Bot
    } else if TABLET.is_match(&normalized) {
        DeviceType::Tablet
    } else if MOBILE.is_match(&normalized) {
        DeviceType::Mobile
    } else {
        DeviceType::Desktop
    }
}

fn infer_browser(user_agent: Option<&str>) -> Option<String> {
    let normalized = user_agent?.to_lowercase();
    let has = |needle: &str| normalized.contains(needle);
    let browser = if has("edg/") {
        "Edge"
    } else if has("opr/") {
        "Opera"
    } else if has("firefox/") {
        "Firefox"
    } else if has("electron/") {
        "Electron"
    } else if has("chrome/") || has("crios/") {
        "Chrome"
    } else if has("safari/") && !has("chrome/") {
        "Safari"
    } else {
        return None;
    };
    Some(browser.to_owned())
}

fn infer_os(user_agent: Option<&str>) -> Option<String> {
    let normalized = user_agent?.to_lowercase();
    let has = |needle: &str| normalized.contains(needle);
    let os = if has("iphone") || has("ipad") || has("ipod") {
        "iOS"
    } else if has("android") {
        "Android"
    } else if has("mac os x") || has("macintosh") {
        "macOS"
    } else if has("windows nt") {
        "Windows"
    } else if has("linux") {
        "Linux"
    } else {
        return None;
    };
    Some(os.to_owned())
}

/// `deriveAuthClientMetadata`.
pub fn derive_auth_client_metadata(user_agent: Option<&str>, remote_address: Option<&str>, presented: Option<&PresentedClient>) -> AuthClientMetadata {
    let user_agent = non_empty_trimmed(user_agent);
    let ip_address = normalize_ip_address(remote_address);
    let os = presented.and_then(|p| p.os.clone()).or_else(|| infer_os(user_agent.as_deref()));
    let browser = infer_browser(user_agent.as_deref());
    AuthClientMetadata {
        label: presented.and_then(|p| p.label.clone()).filter(|label| !label.is_empty()),
        ip_address,
        device_type: presented
            .and_then(|p| p.device_type)
            .unwrap_or_else(|| infer_device_type(user_agent.as_deref())),
        user_agent,
        os,
        browser,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // utils.test.ts "deriveAuthClientMetadata"
    #[test]
    fn labels_electron_user_agents_as_electron() {
        let metadata = derive_auth_client_metadata(
            Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) t3code/0.0.15 Chrome/136.0.7103.93 Electron/36.3.2 Safari/537.36"),
            Some("::ffff:127.0.0.1"),
            None,
        );
        assert_eq!(metadata.browser.as_deref(), Some("Electron"));
        assert_eq!(metadata.device_type, DeviceType::Desktop);
        assert_eq!(metadata.ip_address.as_deref(), Some("127.0.0.1"));
        assert_eq!(metadata.os.as_deref(), Some("macOS"));
        assert_eq!(metadata.label, None);
    }

    #[test]
    fn presented_identity_does_not_replace_transport_metadata() {
        let metadata = derive_auth_client_metadata(
            Some("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/136.0.7103.93 Electron/36.3.2 Safari/537.36"),
            Some("::ffff:192.168.213.72"),
            Some(&PresentedClient {
                label: Some("Synthetic Mobile".into()),
                device_type: Some(DeviceType::Mobile),
                os: Some("iOS".into()),
            }),
        );
        assert_eq!(metadata.label.as_deref(), Some("Synthetic Mobile"));
        assert_eq!(metadata.browser.as_deref(), Some("Electron"));
        assert_eq!(metadata.device_type, DeviceType::Mobile);
        assert_eq!(metadata.ip_address.as_deref(), Some("192.168.213.72"));
        assert_eq!(metadata.os.as_deref(), Some("iOS"));
        assert!(metadata.user_agent.unwrap().contains("Electron/36.3.2"));
    }

    #[test]
    fn infers_phones_tablets_and_bots() {
        let iphone = derive_auth_client_metadata(
            Some("Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Mobile/15E148 Safari/604.1"),
            None,
            None,
        );
        assert_eq!(iphone.device_type, DeviceType::Mobile);
        assert_eq!(iphone.os.as_deref(), Some("iOS"));
        assert_eq!(iphone.browser.as_deref(), Some("Safari"));
        assert_eq!(derive_auth_client_metadata(Some("curl/8.0"), None, None).device_type, DeviceType::Bot);
        assert_eq!(derive_auth_client_metadata(Some("x iPad y"), None, None).device_type, DeviceType::Tablet);
        assert_eq!(derive_auth_client_metadata(Some("  "), None, None).device_type, DeviceType::Unknown);
        assert_eq!(derive_auth_client_metadata(None, Some(" "), None).ip_address, None);
    }
}
